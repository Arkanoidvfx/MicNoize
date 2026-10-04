#include "audio.hpp"
#include <audioclient.h>
#include <mmdeviceapi.h>
#include <endpointvolume.h>
#include <functiondiscoverykeys_devpkey.h>
#include <avrt.h>
#include <wrl/client.h>
#include <wrl/implements.h>
#include <audioclientactivationparams.h>
#include <tlhelp32.h>
#include <winhttp.h>
#include <nvAudioEffects.h>
#include <nvAFXDenoiser.h>
#include <cmath>
#include <chrono>
#include <stdexcept>
#include <fstream>
#include <iostream>
#include <memory>
#include <future>
#include <xmmintrin.h>
#include "tag_link.hpp"
#include "tag_endpoint.hpp"
#include "tag.hpp"
#include "effects.hpp"

namespace mic {
using Microsoft::WRL::ComPtr;
// Windows invalidates an endpoint when it is unplugged, disabled or switches format.
struct DeviceLost : std::runtime_error {using std::runtime_error::runtime_error;};
static void check(HRESULT hr, const char* where) {
    if(FAILED(hr)) {
        char text[160]; sprintf_s(text, "%s: HRESULT 0x%08lX", where, static_cast<unsigned long>(hr));
        if(hr==AUDCLNT_E_DEVICE_INVALIDATED) throw DeviceLost(text);
        throw std::runtime_error(text);
    }
}
struct Com {
    Com() { check(CoInitializeEx(nullptr, COINIT_MULTITHREADED), "COM initialization"); }
    ~Com() { CoUninitialize(); }
};
struct Event {
    HANDLE h = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    Event() { if(!h) throw std::runtime_error("CreateEvent failed"); }
    ~Event() { CloseHandle(h); }
};
struct InternetHandle {
    HINTERNET h=nullptr;
    ~InternetHandle() { if(h) WinHttpCloseHandle(h); }
};
constexpr unsigned rvcFrames=rate/2;
static bool rvcRequest(float* input,float* output,unsigned frames,const RvcSettings& config,uint64_t stream) {
    InternetHandle session{WinHttpOpen(L"MicNoize/1.0",WINHTTP_ACCESS_TYPE_NO_PROXY,
        WINHTTP_NO_PROXY_NAME,WINHTTP_NO_PROXY_BYPASS,0)};
    if(!session.h || !WinHttpSetTimeouts(session.h,200,200,1000,3000)) return false;
    InternetHandle connection{WinHttpConnect(session.h,L"127.0.0.1",18889,0)};
    if(!connection.h) return false;
    const auto path=L"/mnr/convert?slot="+std::to_wstring(config.slot)+L"&pitch="+std::to_wstring(config.pitch)+
        L"&index="+std::to_wstring(config.index)+L"&chunk_ms="+std::to_wstring(config.chunkMs)+L"&stream="+std::to_wstring(stream);
    InternetHandle request{WinHttpOpenRequest(connection.h,L"POST",path.c_str(),
        nullptr,WINHTTP_NO_REFERER,WINHTTP_DEFAULT_ACCEPT_TYPES,0)};
    if(!request.h) return false;
    const std::wstring headers=L"Content-Type: application/octet-stream";
    const auto bytes=frames*sizeof(float);
    if(!WinHttpSendRequest(request.h,headers.c_str(),static_cast<DWORD>(headers.size()),input,
        static_cast<DWORD>(bytes),static_cast<DWORD>(bytes),0) || !WinHttpReceiveResponse(request.h,nullptr)) return false;
    DWORD status=0,statusSize=sizeof(status);
    if(!WinHttpQueryHeaders(request.h,WINHTTP_QUERY_STATUS_CODE|WINHTTP_QUERY_FLAG_NUMBER,
        WINHTTP_HEADER_NAME_BY_INDEX,&status,&statusSize,WINHTTP_NO_HEADER_INDEX) || status!=200) return false;
    std::vector<uint8_t> response;
    for(;;) {
        DWORD available=0;
        if(!WinHttpQueryDataAvailable(request.h,&available)) return false;
        if(!available) break;
        if(response.size()+available>bytes) return false;
        const auto start=response.size();response.resize(start+available);DWORD read=0;
        if(!WinHttpReadData(request.h,response.data()+start,available,&read)) return false;
        response.resize(start+read);
    }
    if(response.size()!=bytes) return false;
    memcpy(output,response.data(),bytes);
    for(unsigned i=0;i<frames;++i) {
        auto& sample=output[i];
        if(!std::isfinite(sample)) return false;
        sample=std::clamp(sample,-1.0f,1.0f);
    }
    return true;
}
// The DSP thread owns generations, playout and output_ trimming; the worker only accumulates
// chunks, converts them and pushes exactly as many samples as it consumed (zeros when a chunk is
// skipped or the server fails), so both rings stay index-aligned.
class RvcClient {
    Stats& stats_;
    std::atomic<uint64_t>& config_;
    Ring<65536,RvcSample> input_,output_;
    Event data_;
    std::atomic<bool> enabled_{false},lastFailed_{false};
    std::atomic<unsigned> generation_{0};
    bool active_=false;
    uint64_t previousConfig_=0;
    unsigned seenGeneration_=0;
    RvcPlayout playout_;
    const uint64_t streamBase_=GetTickCount64()<<20;
    std::jthread worker_; // Start only after all state has been initialized.
    void run(std::stop_token stop) {
        std::array<float,rvcFrames> chunk{},output{};
        std::array<RvcSample,rvcFrames> converted{};
        unsigned filled=0,chunkGeneration=0,skips=0;
        RvcSample s;
        // Every consumed chunk produces exactly `frames` output samples, silence included.
        auto publish=[&](unsigned frames,bool ok){
            for(unsigned i=0;i<frames;++i)converted[i]={ok?output[i]:0.0f,chunkGeneration};
            if(!output_.push(converted.data(),frames))generation_.fetch_add(1); // Consumer resets; only it may trim output_.
        };
        while(!stop.stop_requested()) {
            WaitForSingleObject(data_.h,INFINITE);
            if(!enabled_) {while(input_.pop(&s,1)){}continue;}
            const auto config=RvcSettings::unpack(config_.load());
            const auto frames=config.chunkMs*48;
            while(!stop.stop_requested() && input_.pop(&s,1)) {
                // A generation change restarts the chunk at index 0 of the new stream.
                if(s.generation!=chunkGeneration){filled=0;chunkGeneration=s.generation;}
                chunk[filled++]=std::clamp(s.sample*config.gain/100.0f,-1.0f,1.0f);
                if(filled<frames)continue;
                filled=0;
                if(chunkGeneration!=generation_.load())continue; // Stale chunk: no inference for audio nobody will play.
                if(input_.size()>=rvcSlack){publish(frames,false);++skips;continue;} // Would miss the playout deadline anyway.
                const auto begin=std::chrono::steady_clock::now();
                // A new stream id after a skip makes the server reset its crossfade instead of joining across the gap.
                const bool ok=rvcRequest(chunk.data(),output.data(),frames,config,streamBase_+(static_cast<uint64_t>(chunkGeneration)<<8)+(skips&255));
                stats_.rvcLatencyMs=std::chrono::duration<float,std::milli>(std::chrono::steady_clock::now()-begin).count();
                lastFailed_=!ok;
                publish(frames,ok);
                if(chunkGeneration!=generation_.load())break;
            }
        }
    }
public:
    explicit RvcClient(Stats& stats,std::atomic<uint64_t>& config):stats_(stats),config_(config),worker_([this](std::stop_token stop){run(stop);}) {}
    ~RvcClient() {worker_.request_stop();SetEvent(data_.h);worker_.join();}
    void process(float* samples,unsigned count,bool enabled,uint8_t* modified=nullptr) {
        enabled_=enabled;
        const auto config=config_.load();
        if(enabled!=active_ || config!=previousConfig_) {
            previousConfig_=config;active_=enabled;
            generation_.fetch_add(1);lastFailed_=false;SetEvent(data_.h);
            stats_.rvcState=enabled?1:0;stats_.rvcLatencyMs=0;
        }
        if(!enabled) return; // Exact dry bypass.
        const auto generation=generation_.load();
        if(generation!=seenGeneration_){seenGeneration_=generation;output_.trim(0);} // Before this generation's first push, so no fresh chunk can be trimmed.
        std::array<RvcSample,block> tagged{};
        const auto n=std::min(count,block);
        for(unsigned i=0;i<n;++i)tagged[i]={samples[i],generation};
        if(!input_.push(tagged.data(),n)) {
            generation_.fetch_add(1);SetEvent(data_.h);
            std::fill_n(samples,count,0.0f);if(modified)std::fill_n(modified,count,static_cast<uint8_t>(ModifiedEffects));
            stats_.rvcState=3;return;
        }
        SetEvent(data_.h);
        const int state=playout_.process(output_,samples,n,modified,RvcSettings::unpack(config).chunkMs*48+rvcSlack,generation);
        stats_.rvcState=lastFailed_?3:state;
    }
};
void checkRvcIdle() {
    Stats stats;std::atomic<uint64_t> config{RvcSettings{}.packed()};
    auto rvc=std::make_unique<RvcClient>(stats,config);
    std::array<float,block> samples{};samples.fill(0.125f);const auto dry=samples;
    for(int pass=0;pass<2;++pass) {
        rvc->process(samples.data(),block,false);
        if(samples!=dry || stats.rvcState!=0)throw std::runtime_error("Disabled RVC changed audio/state");
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(150));
    const auto begin=std::chrono::steady_clock::now();
    rvc.reset(); // Destructor must wake the indefinitely sleeping worker.
    if(std::chrono::steady_clock::now()-begin>std::chrono::seconds(1))throw std::runtime_error("Idle RVC shutdown stalled");
    std::cout<<"RVC IDLE CHECK PASSED: dry bypass and sleeping-worker shutdown; no model loaded\n";
}
void checkRvc() {
    Stats stats;std::atomic<uint64_t> config{RvcSettings{}.packed()};auto rvc=std::make_unique<RvcClient>(stats,config);std::array<float,block> samples{};
    const unsigned priming=(RvcSettings{}.chunkMs*48+rvcSlack)/block; // Blocks of exact silence before the fixed delay elapses.
    unsigned converted=0,steady=0,silent=0;
    for(unsigned pass=0;pass<350;++pass) {
        for(unsigned i=0;i<block;++i) samples[i]=static_cast<float>(0.08*std::sin(2*3.141592653589793*220*(pass*block+i)/rate));
        const auto dry=samples;
        rvc->process(samples.data(),block,true);
        const bool zero=std::all_of(samples.begin(),samples.end(),[](float v){return v==0;});
        if(pass<priming && !zero)throw std::runtime_error("RVC leaked audio before its fixed delay elapsed");
        if(pass>=priming+30){++steady;if(zero)++silent;else if(samples!=dry)++converted;}
        for(float v:samples)if(!std::isfinite(v)||std::abs(v)>1)throw std::runtime_error("Invalid RVC worker output");
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    if(converted<steady*8/10 || stats.rvcLatencyMs<=0) throw std::runtime_error("RVC worker mostly returned silence or dry audio");
    samples.fill(0.125f);const auto dry=samples;
    rvc->process(samples.data(),block,false);
    if(samples!=dry || stats.rvcState!=0)throw std::runtime_error("Disabled RVC changed audio");
    std::cout<<"RVC CHECK PASSED: priming_blocks="<<priming<<" converted="<<converted<<'/'<<steady<<" steady blocks, silent="<<silent<<"; disabled bypass\n";
}
struct Mmcss {
    DWORD task = 0;
    HANDLE h = AvSetMmThreadCharacteristicsW(L"Audio", &task);
    ~Mmcss() { if(h) AvRevertMmThreadCharacteristics(h); }
};
std::string utf8(const std::wstring& s) {
    if(s.empty()) return {};
    int n=WideCharToMultiByte(CP_UTF8,0,s.data(),static_cast<int>(s.size()),nullptr,0,nullptr,nullptr);
    std::string out(n,0); WideCharToMultiByte(CP_UTF8,0,s.data(),static_cast<int>(s.size()),out.data(),n,nullptr,nullptr); return out;
}
std::wstring wide(const std::string& s) {
    if(s.empty()) return {};
    int n=MultiByteToWideChar(CP_UTF8,0,s.data(),static_cast<int>(s.size()),nullptr,0);
    std::wstring out(n,0); MultiByteToWideChar(CP_UTF8,0,s.data(),static_cast<int>(s.size()),out.data(),n); return out;
}
std::wstring environment(const wchar_t* name) {
    std::wstring value(32767,0); // Windows' maximum variable length
    const auto n=GetEnvironmentVariableW(name,value.data(),static_cast<DWORD>(value.size()));
    value.resize(n<value.size()?n:0);
    return value;
}
std::filesystem::path projectRoot() {
    if(const auto configured=environment(L"MNR_RUNTIME_ROOT"); !configured.empty())
        return std::filesystem::path(configured);
    std::wstring path(32768,0); auto n=GetModuleFileNameW(nullptr,path.data(),static_cast<DWORD>(path.size()));
    if(!n || n==path.size()) throw std::runtime_error("Cannot resolve executable path");
    path.resize(n);const auto app=std::filesystem::path(path).parent_path(),project=app.parent_path();
    if(app.filename()==L"bin" && std::filesystem::is_directory(project/L"vendor/nvidia-afx-3.0.0")) return project;
    if(const auto roaming=environment(L"APPDATA"); !roaming.empty()) {
        auto components=std::filesystem::path(roaming)/L"Mic Noize/Components";
        if(std::filesystem::is_directory(components/L"vendor")) return components;
    }
    return project;
}
std::vector<Device> devices(bool capture,DWORD states) {
    Com com; ComPtr<IMMDeviceEnumerator> e; ComPtr<IMMDeviceCollection> list;
    check(CoCreateInstance(__uuidof(MMDeviceEnumerator),nullptr,CLSCTX_ALL,IID_PPV_ARGS(&e)),"Enumerate audio devices");
    check(e->EnumAudioEndpoints(capture ? eCapture:eRender,states,&list),"Audio endpoints");
    UINT count=0; check(list->GetCount(&count),"Endpoint count");
    std::vector<Device> out;
    for(UINT i=0;i<count;++i) {
        ComPtr<IMMDevice> d; ComPtr<IPropertyStore> p; LPWSTR id=nullptr;
        check(list->Item(i,&d),"Endpoint"); check(d->GetId(&id),"Endpoint ID");
        Device item{id,L"Audio endpoint"}; CoTaskMemFree(id);
        if(SUCCEEDED(d->OpenPropertyStore(STGM_READ,&p))) {
            PROPVARIANT v; PropVariantInit(&v);
            if(SUCCEEDED(p->GetValue(PKEY_Device_FriendlyName,&v)) && v.vt==VT_LPWSTR) item.name=v.pwszVal;
            PropVariantClear(&v);
        }
        out.push_back(std::move(item));
    }
    // The default communications microphone goes first: a fresh install on another PC must be
    // able to select some microphone, not only a HyperX.
    if(capture && states==DEVICE_STATE_ACTIVE) {
        ComPtr<IMMDevice> preferred; LPWSTR id=nullptr;
        if(SUCCEEDED(e->GetDefaultAudioEndpoint(eCapture,eCommunications,&preferred))
           && SUCCEEDED(preferred->GetId(&id))) {
            const std::wstring wanted=id; CoTaskMemFree(id);
            const auto it=std::find_if(out.begin(),out.end(),[&](const Device& d){return d.id==wanted;});
            if(it!=out.end()) std::rotate(out.begin(),it,it+1);
        }
    }
    return out;
}

Device tagMicrophone() {
    TagEndpointStatus status;
    if(!readTagEndpointStatus(status))throw std::runtime_error("Обновите фоновый хост Mic Noize: отсутствует статус устройства");
    if(!status.ready)throw std::runtime_error(status.error[0]?status.error:"TAG endpoint controller unavailable");
    for(const auto& device:devices(true))if(device.id==status.endpoint)return device;
    throw std::runtime_error("Bound Mic Noize TAG microphone endpoint is not active");
}

static ComPtr<IAudioEndpointVolume> tagEndpointLevel() {
    const auto device=tagMicrophone();
    ComPtr<IMMDeviceEnumerator> enumerator; ComPtr<IMMDevice> endpoint; ComPtr<IAudioEndpointVolume> level;
    check(CoCreateInstance(__uuidof(MMDeviceEnumerator),nullptr,CLSCTX_ALL,IID_PPV_ARGS(&enumerator)),"TAG level enumerator");
    check(enumerator->GetDevice(device.id.c_str(),&endpoint),"TAG level endpoint");
    check(endpoint->Activate(__uuidof(IAudioEndpointVolume),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(level.GetAddressOf())),"TAG level control");
    return level;
}

float holdTagEndpointLevel(IAudioEndpointVolume* level,bool unmute,const GUID* context) {
    float minimum=0,maximum=0,step=0;
    check(level->GetVolumeRange(&minimum,&maximum,&step),"TAG level range");
    if(!std::isfinite(minimum) || !std::isfinite(maximum) || !std::isfinite(step) || minimum>0 || maximum<0 || maximum>120 || step<=0)
        throw std::runtime_error("Invalid TAG level range");
    UINT channels=0; check(level->GetChannelCount(&channels),"TAG level channels");
    if(!channels || channels>32) throw std::runtime_error("Invalid TAG level channel count");
    for(UINT channel=0;channel<channels;++channel) {
        float current=0;check(level->GetChannelVolumeLevel(channel,&current),"TAG channel level");
        if(!std::isfinite(current) || std::abs(current-maximum)>0.1f) check(level->SetChannelVolumeLevel(channel,maximum,context),"TAG lock channel level");
    }
    float current=0;check(level->GetMasterVolumeLevel(&current),"TAG current level");
    if(!std::isfinite(current) || std::abs(current-maximum)>0.1f) check(level->SetMasterVolumeLevel(maximum,context),"TAG lock master level");
    BOOL mute=FALSE;check(level->GetMute(&mute),"TAG current mute");
    if(mute && unmute) check(level->SetMute(FALSE,context),"TAG unmute virtual microphone");
    float scalar=0;check(level->GetMasterVolumeLevelScalar(&scalar),"TAG verify master level");
    if(!std::isfinite(scalar) || scalar<0.9999f)throw std::runtime_error("TAG master level did not reach 100%");
    for(UINT channel=0;channel<channels;++channel) {
        check(level->GetChannelVolumeLevelScalar(channel,&scalar),"TAG verify channel level");
        if(!std::isfinite(scalar) || scalar<0.9999f)throw std::runtime_error("TAG channel level did not reach 100%");
    }
    check(level->GetMute(&mute),"TAG verify mute");
    if(unmute && mute)throw std::runtime_error("TAG endpoint remained muted");
    return std::pow(10.0f,-maximum/20.0f);
}

void checkTagLevel() {
    Com com;auto level=tagEndpointLevel();
    float original=0;BOOL originalMute=FALSE;UINT count=0;
    check(level->GetMasterVolumeLevel(&original),"TAG test original level");
    check(level->GetMute(&originalMute),"TAG test original mute");
    check(level->GetChannelCount(&count),"TAG test channels");
    std::vector<float> channels(count);
    for(UINT i=0;i<count;++i) check(level->GetChannelVolumeLevel(i,&channels[i]),"TAG test channel");
    float compensation=0,locked=0;
    {
        struct Restore {
            IAudioEndpointVolume* level;float master;BOOL mute;const std::vector<float>& channels;
            ~Restore() {
                for(UINT i=0;i<channels.size();++i) level->SetChannelVolumeLevel(i,channels[i],nullptr);
                level->SetMasterVolumeLevel(master,nullptr);level->SetMute(mute,nullptr);
            }
        } restore{level.Get(),original,originalMute,channels};
        check(level->SetMute(TRUE,nullptr),"TAG test mute");
        check(level->SetMasterVolumeLevel(0,nullptr),"TAG test change level");
        compensation=holdTagEndpointLevel(level.Get(),false);
        check(level->GetMasterVolumeLevel(&locked),"TAG test locked level");
        if(std::abs(compensation*std::pow(10.0f,locked/20.0f)-1.0f)>0.01f)
            throw std::runtime_error("TAG level compensation mismatch");
        for(UINT i=0;i<count;++i) {
            float channel=0;check(level->GetChannelVolumeLevel(i,&channel),"TAG test locked channel");
            if(std::abs(channel-locked)>0.1f) throw std::runtime_error("TAG channel level not locked");
        }
    }
    float restored=0;BOOL restoredMute=FALSE;
    check(level->GetMasterVolumeLevel(&restored),"TAG test restored level");
    check(level->GetMute(&restoredMute),"TAG test restored mute");
    if(std::abs(restored-original)>0.1f || restoredMute!=originalMute) throw std::runtime_error("TAG level was not restored");
    for(UINT i=0;i<count;++i) {
        float channel=0;check(level->GetChannelVolumeLevel(i,&channel),"TAG test restored channel");
        if(std::abs(channel-channels[i])>0.1f) throw std::runtime_error("TAG channel level was not restored");
    }
    std::cout<<"PASS: TAG max="<<locked<<" dB compensation="<<compensation<<"; original level restored\n";
}

void checkTagLevelWatch() {
    Com com;auto level=tagEndpointLevel();
    float minimum=0,maximum=0,step=0;
    check(level->GetVolumeRange(&minimum,&maximum,&step),"TAG watch test range");
    try {
        check(level->SetMute(TRUE,nullptr),"TAG watch test mute");
        check(level->SetMasterVolumeLevel(0,nullptr),"TAG watch test level");
        float current=0;BOOL mute=TRUE;
        for(unsigned attempt=0;attempt<20;++attempt) {
            Sleep(50);
            check(level->GetMasterVolumeLevel(&current),"TAG watch test current level");
            check(level->GetMute(&mute),"TAG watch test current mute");
            if(std::abs(current-maximum)<=0.1f && !mute)break;
        }
        if(std::abs(current-maximum)>0.1f || mute) throw std::runtime_error("TAG level guard did not restore 100% and unmute");
        UINT channels=0;check(level->GetChannelCount(&channels),"TAG watch channel count");
        if(!channels || channels>32)throw std::runtime_error("Invalid TAG watch channel count");
        for(UINT channel=0;channel<channels;++channel) {
            check(level->SetChannelVolumeLevelScalar(channel,0.25f,nullptr),"TAG watch change channel");
            float scalar=0;
            for(unsigned attempt=0;attempt<20;++attempt) {
                Sleep(50);check(level->GetChannelVolumeLevelScalar(channel,&scalar),"TAG watch verify channel");
                if(std::isfinite(scalar) && scalar>=0.9999f)break;
            }
            if(!std::isfinite(scalar) || scalar<0.9999f)throw std::runtime_error("TAG guard did not restore channel to 100%");
        }
        std::cout<<"PASS: TAG guard restored master, "<<channels<<" channels and unmuted after external changes\n";
    } catch(...) {level->SetMute(FALSE,nullptr);throw;}
}

std::string gpuArch(std::string& name) {
    // The driver's CUDA API is present with every NVIDIA driver; no CUDA runtime is needed.
    HMODULE cuda=LoadLibraryExW(L"nvcuda.dll",nullptr,LOAD_LIBRARY_SEARCH_SYSTEM32);
    if(!cuda) throw std::runtime_error("Драйвер NVIDIA не найден: нужна видеокарта NVIDIA RTX");
    struct Free {HMODULE h;~Free(){FreeLibrary(h);}} guard{cuda};
    using Init=int(__stdcall*)(unsigned);using Get=int(__stdcall*)(int*,int);
    using Attribute=int(__stdcall*)(int*,int,int);using Name=int(__stdcall*)(char*,int,int);
    const auto init=reinterpret_cast<Init>(GetProcAddress(cuda,"cuInit"));
    const auto get=reinterpret_cast<Get>(GetProcAddress(cuda,"cuDeviceGet"));
    const auto attribute=reinterpret_cast<Attribute>(GetProcAddress(cuda,"cuDeviceGetAttribute"));
    const auto getName=reinterpret_cast<Name>(GetProcAddress(cuda,"cuDeviceGetName"));
    int device=0,major=0,minor=0;char text[256]{};
    // 75/76: CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR/MINOR.
    if(!init || !get || !attribute || !getName || init(0) || get(&device,0) ||
       attribute(&major,75,device) || attribute(&minor,76,device) || getName(text,255,device))
        throw std::runtime_error("CUDA недоступна: обновите драйвер NVIDIA");
    name=text;
    const int sm=major*10+minor;
    if(sm>=100) return "blackwell";
    if(sm==89) return "ada";
    if(sm>=80 && sm<89) return "ampere";
    if(sm==75) return "turing";
    throw std::runtime_error(name+" (sm "+std::to_string(sm)+") не поддерживается NVIDIA Audio Effects: нужна RTX");
}

static std::atomic<const CpuDenoiserApi*> cpuDenoiser{nullptr};
void setCpuDenoiser(const CpuDenoiserApi* api) {cpuDenoiser=api;}

class Afx {
    HMODULE dll_=nullptr;
    std::vector<DLL_DIRECTORY_COOKIE> dirs_;
    NvAFX_Handle handle_=nullptr;
    template<class T> T proc(const char* name) {
        auto p=GetProcAddress(dll_,name);
        if(!p) throw std::runtime_error(std::string("Missing NVIDIA export: ")+name);
        return reinterpret_cast<T>(p);
    }
    static void ok(NvAFX_Status s, const char* where) {
        if(s!=NVAFX_STATUS_SUCCESS) throw std::runtime_error(std::string(where)+": NVIDIA status "+std::to_string(s));
    }
    decltype(&NvAFX_DestroyEffect) destroy_=nullptr;
    decltype(&NvAFX_Run) run_=nullptr;
    decltype(&NvAFX_Reset) reset_=nullptr;
    decltype(&NvAFX_SetFloat) setFloat_=nullptr;
    void release() {
        if(handle_ && destroy_) destroy_(handle_);
        if(dll_) FreeLibrary(dll_);
        for(auto d:dirs_) RemoveDllDirectory(d);
    }
public:
    explicit Afx(const Config& c) {
        try {
            if(!SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32|LOAD_LIBRARY_SEARCH_USER_DIRS))
                throw std::runtime_error("Cannot configure Windows DLL search path");
            const auto root=std::filesystem::absolute(c.sdk);
            // TensorRT models are built per GPU architecture; another one fails in NvAFX_Load.
            std::string gpu;const auto arch=gpuArch(gpu);
            const auto model=root/L"features/nvafxdenoiser/models"/arch/(c.version==1?L"denoiser_48k.trtpkg":L"denoiser_v2_48k.trtpkg");
            if(!std::filesystem::is_regular_file(model))
                throw std::runtime_error("Нет модели NVIDIA для "+gpu+" ("+arch+"): перезапустите Mic Noize, чтобы докачать её");
            // All proprietary runtime files stay in the official, unmodified SDK tree.
            for(auto sub:{L"bin",L"bin/external/cuda/bin",L"bin/external/nvtrt/bin",L"bin/external/openssl/bin",L"features/nvafxdenoiser/bin"}) {
                auto cookie=AddDllDirectory((root/sub).c_str());
                if(!cookie) throw std::runtime_error("Cannot register SDK DLL directory");
                dirs_.push_back(cookie);
            }
            // Stop unloads the SDK, so the logger is set up on every fresh load, not once.
            const bool fresh=!GetModuleHandleW(L"NVAudioEffects.dll");
            dll_=LoadLibraryExW((root/L"bin/NVAudioEffects.dll").c_str(),nullptr,LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR|LOAD_LIBRARY_SEARCH_DEFAULT_DIRS);
            if(!dll_) throw std::runtime_error("Cannot load NVIDIA SDK DLL; Windows error "+std::to_string(GetLastError()));
            // SDK errors (why NvAFX_Load failed) go to results/nvafx.log; the SDK takes a narrow path.
            using InitLogger=NvAFX_Status(*)(LoggingSeverity,LoggingTarget,const char*,logging_cb_t,void*);
            if(const auto init=reinterpret_cast<InitLogger>(GetProcAddress(dll_,"NvAFX_InitializeLogger")); fresh && init) {
                const auto folder=projectRoot()/L"results";std::error_code ignored;std::filesystem::create_directories(folder,ignored);
                const auto path=utf8((folder/L"nvafx.log").wstring());
                if(std::none_of(path.begin(),path.end(),[](unsigned char ch){return ch>127;}))
                    init(LOG_LEVEL_ERROR,LOG_TARGET_FILE,path.c_str(),nullptr,nullptr);
            }
            destroy_=proc<decltype(destroy_)>("NvAFX_DestroyEffect");
            run_=proc<decltype(run_)>("NvAFX_Run"); reset_=proc<decltype(reset_)>("NvAFX_Reset");
            setFloat_=proc<decltype(setFloat_)>("NvAFX_SetFloat");
            auto create=proc<decltype(&NvAFX_CreateEffect)>("NvAFX_CreateEffect");
            auto setU32=proc<decltype(&NvAFX_SetU32)>("NvAFX_SetU32");
            auto setString=proc<decltype(&NvAFX_SetString)>("NvAFX_SetString");
            auto getU32=proc<decltype(&NvAFX_GetU32)>("NvAFX_GetU32");
            ok(create(NVAFX_EFFECT_DENOISER,&handle_),"CreateEffect");
            ok(setU32(handle_,NVAFX_PARAM_USE_DEFAULT_GPU,1),"Select GPU");
            ok(setU32(handle_,NVAFX_PARAM_INPUT_SAMPLE_RATE,rate),"Input sample rate");
            ok(setU32(handle_,NVAFX_PARAM_OUTPUT_SAMPLE_RATE,rate),"Output sample rate");
            ok(setU32(handle_,NVAFX_PARAM_EFFECT_VERSION,c.version),"Effect version");
            if(c.cudaGraphs>=0) ok(setU32(handle_,NVAFX_PARAM_DISABLE_CUDA_GRAPH,c.cudaGraphs?0:1),"CUDA graph setting");
            // NVIDIA's Windows API takes a narrow path; reject non-ASCII rather than misload it.
            auto modelText=utf8(model.wstring());
            if(std::any_of(modelText.begin(),modelText.end(),[](unsigned char ch){return ch>127;}))
                throw std::runtime_error("Place the NVIDIA SDK in a path with ASCII characters");
            ok(setString(handle_,NVAFX_PARAM_MODEL_PATH,modelText.c_str()),"Model path");
            strength(c.intensity);
            ok(proc<decltype(&NvAFX_Load)>("NvAFX_Load")(handle_),("Load model "+arch+", "+gpu).c_str());
            for(auto param:{NVAFX_PARAM_NUM_SAMPLES_PER_INPUT_FRAME,NVAFX_PARAM_NUM_SAMPLES_PER_OUTPUT_FRAME}) {
                unsigned n=0; ok(getU32(handle_,param,&n),param);
                if(n!=block) throw std::runtime_error("This application requires 480-sample NVIDIA frames");
            }
            for(auto param:{NVAFX_PARAM_NUM_INPUT_CHANNELS,NVAFX_PARAM_NUM_OUTPUT_CHANNELS}) {
                unsigned n=0; ok(getU32(handle_,param,&n),param);
                if(n!=1) throw std::runtime_error("This application requires a mono NVIDIA effect");
            }
        } catch(...) { release(); throw; }
    }
    ~Afx() { release(); }
    void strength(float f) { ok(setFloat_(handle_,NVAFX_PARAM_INTENSITY_RATIO,f),f>1?"NVIDIA rejected experimental intensity above 100%; return to 100%":"Intensity"); }
    void reset() { ok(reset_(handle_),"Reset effect"); }
    void process(const float* in,float* out) { ok(run_(handle_,&in,&out,block,1),"Process audio"); }
};

void benchmarkAfx(const Config& config,const std::vector<float>& samples,unsigned seconds,const std::filesystem::path& csv,bool churn) {
    if(samples.size()<block || samples.size()>rate*600 || !seconds || seconds>600 || config.cudaGraphs < -1 || config.cudaGraphs>1)
        throw std::runtime_error("Invalid benchmark input");
    for(float v:samples) if(!std::isfinite(v) || std::abs(v)>1) throw std::runtime_error("Invalid PCM sample");
    if(std::filesystem::exists(csv)) throw std::runtime_error("Benchmark report already exists");
    struct Timing {double run, cpu, late;};
    std::vector<Timing> timings; timings.reserve(seconds*100);
    std::vector<float> rendered; rendered.reserve(seconds*rate);
    auto cpu=[] {
        FILETIME created,ended,kernel,user;
        if(!GetThreadTimes(GetCurrentThread(),&created,&ended,&kernel,&user)) throw std::runtime_error("Thread timing failed");
        return ((static_cast<uint64_t>(kernel.dwHighDateTime)<<32)|kernel.dwLowDateTime)+
               ((static_cast<uint64_t>(user.dwHighDateTime)<<32)|user.dwLowDateTime);
    };
    Afx fx(config); Mmcss priority;
    std::array<float,block> in{},out{};
    for(unsigned i=0;i<20;++i) fx.process(samples.data()+(i%(samples.size()/block))*block,out.data());
    fx.reset();
    using Clock=std::chrono::steady_clock;
    HANDLE timer=CreateWaitableTimerExW(nullptr,nullptr,CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,TIMER_ALL_ACCESS);
    if(!timer) throw std::runtime_error("Benchmark timer creation failed");
    struct Timer {HANDLE h; ~Timer(){CloseHandle(h);}} closeTimer{timer};
    auto due=Clock::now();
    for(unsigned frame=0;frame<seconds*100;++frame) {
        const auto remaining=std::chrono::duration_cast<std::chrono::nanoseconds>(due-Clock::now()).count();
        if(remaining>0) {
            LARGE_INTEGER delay; delay.QuadPart=-std::max<int64_t>(1,remaining/100);
            if(!SetWaitableTimer(timer,&delay,0,nullptr,nullptr,FALSE) || WaitForSingleObject(timer,2000)!=WAIT_OBJECT_0)
                throw std::runtime_error("Benchmark timer failed");
        }
        // Repeat identical 5-second silence/speech transitions in each A/B arm.
        if((frame/500)%2==0) in.fill(0);
        else std::copy_n(samples.data()+((frame%500)%(samples.size()/block))*block,block,in.data());
        // Churn: what the alternate-intensity hold and capture discontinuities do in a session;
        // the next block's run time shows whether the SDK rebuilds state after them.
        if(churn && frame && frame%250==0) fx.reset();
        else if(churn && frame && frame%100==0) fx.strength((frame/100)%2?0.5f:config.intensity);
        const auto beforeCpu=cpu(); const auto begin=Clock::now();
        fx.process(in.data(),out.data());
        const auto end=Clock::now(); const auto afterCpu=cpu();
        timings.push_back({std::chrono::duration<double,std::milli>(end-begin).count(),(afterCpu-beforeCpu)/10000.0,
            std::chrono::duration<double,std::milli>(begin-due).count()});
        for(float v:out) if(!std::isfinite(v)) throw std::runtime_error("Non-finite benchmark output");
        rendered.insert(rendered.end(),out.begin(),out.end());
        due+=std::chrono::milliseconds(10);
    }
    std::ofstream report(csv); report<<"frame,run_ms,thread_cpu_ms,start_late_ms\n";
    for(size_t i=0;i<timings.size();++i) {auto& t=timings[i];report<<i<<','<<t.run<<','<<t.cpu<<','<<t.late<<'\n';}
    std::ofstream pcm(csv.wstring()+L".f32",std::ios::binary);
    pcm.write(reinterpret_cast<const char*>(rendered.data()),rendered.size()*sizeof(float));
    if(!report || !pcm) throw std::runtime_error("Benchmark report write failed");
}

struct Stream {
    ComPtr<IAudioClient> client;
    unsigned channels=0, capacity=0;
    float periodMs=0;
    bool started=false;
    ~Stream() { if(started) client->Stop(); }
    void open(const std::wstring& id,bool capture,HANDLE event,unsigned requestedMs) {
        ComPtr<IMMDeviceEnumerator> e; ComPtr<IMMDevice> d;
        check(CoCreateInstance(__uuidof(MMDeviceEnumerator),nullptr,CLSCTX_ALL,IID_PPV_ARGS(&e)),"Audio enumerator");
        check(e->GetDevice(id.c_str(),&d),"Selected endpoint disconnected");
        auto activate=[&] { client.Reset(); check(d->Activate(__uuidof(IAudioClient),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(client.GetAddressOf())),"Activate audio endpoint"); };
        activate();
        WAVEFORMATEX* mix=nullptr; check(client->GetMixFormat(&mix),"Mix format");
        channels=mix->nChannels; CoTaskMemFree(mix);
        if(channels==0 || channels>32) throw std::runtime_error("Unsupported endpoint channel count");
        WAVEFORMATEX format{};
        format.wFormatTag=WAVE_FORMAT_IEEE_FLOAT; format.nChannels=static_cast<WORD>(channels);
        format.nSamplesPerSec=rate; format.wBitsPerSample=32;
        format.nBlockAlign=static_cast<WORD>(channels*sizeof(float)); format.nAvgBytesPerSec=rate*format.nBlockAlign;
        DWORD flags=AUDCLNT_STREAMFLAGS_EVENTCALLBACK|AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM|AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if(!capture) flags|=AUDCLNT_STREAMFLAGS_RATEADJUST;
        ComPtr<IAudioClient3> low; HRESULT initialized=E_FAIL;
        if(SUCCEEDED(client.As(&low))) {
            UINT32 def=0,fund=0,min=0,max=0;
            if(SUCCEEDED(low->GetSharedModeEnginePeriod(&format,&def,&fund,&min,&max)) && fund) {
                auto wanted=std::clamp(((requestedMs*48+fund-1)/fund)*fund,min,max);
                // IAudioClient3 uses the negotiated format; conversion flags belong to legacy Initialize.
                DWORD lowFlags=AUDCLNT_STREAMFLAGS_EVENTCALLBACK|(capture?0:AUDCLNT_STREAMFLAGS_RATEADJUST);
                initialized=low->InitializeSharedAudioStream(lowFlags,wanted,&format,nullptr);
                if(SUCCEEDED(initialized)) periodMs=1000.0f*wanted/rate;
            }
        }
        if(FAILED(initialized)) {
            low.Reset(); activate();
            check(client->Initialize(AUDCLNT_SHAREMODE_SHARED,flags,200000,0,&format,nullptr),"Initialize shared 48 kHz stream");
            REFERENCE_TIME p=0; check(client->GetDevicePeriod(&p,nullptr),"Device period"); periodMs=static_cast<float>(p)/10000;
        }
        check(client->GetBufferSize(&capacity),"Audio buffer size");
        check(client->SetEventHandle(event),"Audio event");
    }
    void start() { check(client->Start(),"Start audio stream"); started=true; }
};
// Discord's main process owns its audio subprocesses. Capture that tree only.
static DWORD discordProcess() {
    HANDLE snapshot=CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS,0);
    if(snapshot==INVALID_HANDLE_VALUE)throw std::runtime_error("Cannot enumerate Discord processes");
    std::vector<PROCESSENTRY32W> list;PROCESSENTRY32W entry{};entry.dwSize=sizeof(entry);
    if(Process32FirstW(snapshot,&entry))do {
        if(!_wcsicmp(entry.szExeFile,L"Discord.exe")||!_wcsicmp(entry.szExeFile,L"DiscordPTB.exe")||!_wcsicmp(entry.szExeFile,L"DiscordCanary.exe"))list.push_back(entry);
    }while(Process32NextW(snapshot,&entry));
    CloseHandle(snapshot);DWORD pid=0;
    for(const auto& p:list)if(std::none_of(list.begin(),list.end(),[&](const auto& parent){return parent.th32ProcessID==p.th32ParentProcessID;})){
        if(pid)throw std::runtime_error("Оставьте открытой одну версию Discord для захвата звука.");
        pid=p.th32ProcessID;
    }
    if(!pid)throw std::runtime_error("Откройте приложение Discord для его звуковых хоткеев.");
    return pid;
}
class LoopbackActivation final:public Microsoft::WRL::RuntimeClass<Microsoft::WRL::RuntimeClassFlags<Microsoft::WRL::ClassicCom>,IActivateAudioInterfaceCompletionHandler,Microsoft::WRL::FtmBase> {
public:
    Event done;HRESULT result=E_PENDING;ComPtr<IAudioClient> client;
    HRESULT STDMETHODCALLTYPE ActivateCompleted(IActivateAudioInterfaceAsyncOperation* operation) override {
        HRESULT activated=E_FAIL;ComPtr<IUnknown> object;
        result=operation->GetActivateResult(&activated,&object);
        if(SUCCEEDED(result))result=activated;
        if(SUCCEEDED(result))result=object.As(&client);
        SetEvent(done.h);return S_OK;
    }
};
std::wstring Engine::desktopMessage() const {std::lock_guard lock(statusMutex_);return desktopMessage_;}
std::wstring Engine::denoiserMessage() const {std::lock_guard lock(statusMutex_);return denoiserMessage_;}
void Engine::desktopLoop() {
    while(WaitForSingleObject(stop_,0)!=WAIT_OBJECT_0){
        if(!desktopEnabled){stats.desktopState=0;if(WaitForSingleObject(stop_,250)==WAIT_OBJECT_0)break;continue;}
        try {
            stats.desktopState=1;Com com;const auto pid=discordProcess();
            HANDLE process=OpenProcess(SYNCHRONIZE,FALSE,pid);
            if(!process)throw std::runtime_error("Discord завершился; ожидаем перезапуска.");
            struct CloseProcess{HANDLE h;~CloseProcess(){CloseHandle(h);}}guard{process};
            auto completion=Microsoft::WRL::Make<LoopbackActivation>();
            if(!completion)throw std::runtime_error("Cannot allocate loopback activation");
            AUDIOCLIENT_ACTIVATION_PARAMS params{};params.ActivationType=AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK;
            params.ProcessLoopbackParams.TargetProcessId=pid;
            params.ProcessLoopbackParams.ProcessLoopbackMode=PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE;
            PROPVARIANT prop{};prop.vt=VT_BLOB;prop.blob.cbSize=sizeof(params);prop.blob.pBlobData=reinterpret_cast<BYTE*>(&params);
            ComPtr<IActivateAudioInterfaceAsyncOperation> activation;
            check(ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,__uuidof(IAudioClient),&prop,completion.Get(),&activation),"Activate Discord capture");
            HANDLE activationEvents[]={stop_,completion->done.h};
            const auto activated=WaitForMultipleObjects(2,activationEvents,FALSE,5000);
            if(activated==WAIT_OBJECT_0)break;
            if(activated!=WAIT_OBJECT_0+1)throw std::runtime_error("Discord capture activation timed out");
            check(completion->result,"Discord capture activation");
            Stream input;input.client=completion->client;input.channels=2;Event event;
            WAVEFORMATEX format{WAVE_FORMAT_IEEE_FLOAT,2,rate,rate*8,8,32,0};
            check(input.client->Initialize(AUDCLNT_SHAREMODE_SHARED,AUDCLNT_STREAMFLAGS_LOOPBACK|AUDCLNT_STREAMFLAGS_EVENTCALLBACK|AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM|AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,0,0,&format,nullptr),"Initialize Discord loopback");
            check(input.client->GetBufferSize(&input.capacity),"Discord capture capacity");
            check(input.client->SetEventHandle(event.h),"Discord capture event");
            ComPtr<IAudioCaptureClient> capture;check(input.client->GetService(IID_PPV_ARGS(&capture)),"Discord capture service");
            std::vector<float> mono(std::max(input.capacity,block));
            input.start();stats.desktopState=2;
            {std::lock_guard lock(statusMutex_);desktopMessage_=L"Discord · готов";}
            HANDLE events[]={stop_,process,event.h};
            while(desktopEnabled){
                const auto wait=WaitForMultipleObjects(3,events,FALSE,100);
                if(wait==WAIT_OBJECT_0)break;
                if(wait==WAIT_OBJECT_0+1)throw std::runtime_error("Discord закрыт; ожидаем перезапуска.");
                if(wait==WAIT_FAILED)throw std::runtime_error("Discord capture wait failed");
                UINT32 n=0;check(capture->GetNextPacketSize(&n),"Discord packet");
                while(n){
                    BYTE* bytes=nullptr;DWORD flags=0;check(capture->GetBuffer(&bytes,&n,&flags,nullptr,nullptr),"Discord buffer");
                    if(n>mono.size()){capture->ReleaseBuffer(n);throw std::runtime_error("Discord packet exceeds allocation");}
                    const auto samples=reinterpret_cast<const float*>(bytes);
                    for(unsigned i=0;i<n;++i){const float v=(flags&AUDCLNT_BUFFERFLAGS_SILENT)?0:0.5f*samples[i*2]+0.5f*samples[i*2+1];mono[i]=std::isfinite(v)?std::clamp(v,-1.0f,1.0f):0;}
                    check(capture->ReleaseBuffer(n),"Discord release");
                    desktop_.push(mono.data(),n); // Bounded; DSP drains even when its source is the microphone.
                    check(capture->GetNextPacketSize(&n),"Discord next packet");
                }
            }
            if(stats.desktopState==2)releaseEffects();
            stats.desktopState=0;
        }catch(const std::exception& e){
            {std::lock_guard lock(statusMutex_);desktopMessage_=wide(e.what());}
            if(stats.desktopState==2)releaseEffects();
            stats.desktopState=3;
            if(WaitForSingleObject(stop_,1000)==WAIT_OBJECT_0)break;
        }
    }
    stats.desktopState=0;
}
void checkDiscordCapture(unsigned seconds) {
    if(seconds<1 || seconds>30)throw std::runtime_error("Discord capture check duration must be 1..30 seconds");
    Engine engine;engine.desktopEnabled=true;
    engine.desktopThread_=std::thread([&]{engine.desktopLoop();});
    for(unsigned i=0;i<100 && engine.stats.desktopState!=2;++i){
        if(engine.stats.desktopState==3)throw std::runtime_error(utf8(engine.desktopMessage()));
        Sleep(50);
    }
    if(engine.stats.desktopState!=2)throw std::runtime_error("Discord capture not ready");
    uint64_t frames=0;std::array<float,block> audio{};
    for(unsigned i=0;i<seconds*100;++i){
        if(engine.stats.desktopState!=2)throw std::runtime_error(utf8(engine.desktopMessage()));
        while(engine.desktop_.pop(audio.data(),block)){
            for(float v:audio)if(!std::isfinite(v)||std::abs(v)>1)throw std::runtime_error("Invalid Discord capture sample");
            frames+=block;
        }
        Sleep(10);
    }
    if(!frames)throw std::runtime_error("Discord capture returned no frames");
    engine.stop();std::cout<<"DISCORD CAPTURE PASSED: frames="<<frames<<"; no file or microphone output\n";
}
static float peak(const float* p,size_t n) { float v=0; for(size_t i=0;i<n;++i) v=std::max(v,std::abs(p[i])); return v; }
Monitor::Monitor(Engine& engine):engine_(engine) {
    stop_=CreateEventW(nullptr,TRUE,FALSE,nullptr);
    if(!stop_) throw std::runtime_error("Cannot create monitor stop event");
}
Monitor::~Monitor(){stop();CloseHandle(stop_);}
void Monitor::stop(){engine_.previewMask_=0;SetEvent(stop_);if(thread_.joinable())thread_.join();state=0;}
std::wstring Monitor::message() const {std::lock_guard lock(mutex_);return message_;}
void Monitor::start(const std::wstring& route,uint8_t effectsMask) {
    stop();
    if(!engine_.running()) throw std::runtime_error("Start processing before listening");
    ResetEvent(stop_);frames=0;renderedPeak=0;state=1;
    try {thread_=std::thread([this,route,effectsMask]{
        const bool effectsOnly=effectsMask!=0;
        try {
            Com com;
            // Replugged headphones or a new Windows default output invalidate the stream:
            // reopen on the current default instead of ending the preview.
            for(unsigned lost=0;;) try {
            std::wstring inputId;
            if(route==L"TAG") {
                inputId=tagMicrophone().id;
            }
            if(inputId.empty() && (!effectsOnly || route==L"TAG")) throw std::runtime_error("Прослушивание доступно для TAG. Проверьте подключение виртуального микрофона.");
            ComPtr<IMMDeviceEnumerator> enumerator;ComPtr<IMMDevice> device;LPWSTR rawId=nullptr;
            check(CoCreateInstance(__uuidof(MMDeviceEnumerator),nullptr,CLSCTX_ALL,IID_PPV_ARGS(&enumerator)),"Monitor enumerator");
            check(enumerator->GetDefaultAudioEndpoint(eRender,eConsole,&device),"Выберите наушники устройством вывода Windows");
            check(device->GetId(&rawId),"Monitor output ID");
            const std::wstring outputId=rawId;CoTaskMemFree(rawId);
            std::wstring outputName;
            for(const auto& d:devices(false)) if(d.id==outputId)outputName=d.name;
            auto lower=outputName;std::transform(lower.begin(),lower.end(),lower.begin(),[](wchar_t ch){return static_cast<wchar_t>(towlower(ch));});
            if(outputId==route || tagDriverEndpoint(outputId) || lower.find(L"cable")!=std::wstring::npos || lower.find(L"voicemeeter")!=std::wstring::npos ||
               lower.find(L"nvidia broadcast")!=std::wstring::npos)
                throw std::runtime_error("Для прослушивания выберите наушники выходом Windows, а не виртуальный кабель.");
            Event captureEvent,renderEvent;Stream input,output;
            // TAG needs a capture client to keep its output clock active, even for effects-only preview.
            if(!inputId.empty())input.open(inputId,true,captureEvent.h,5);
            output.open(outputId,false,renderEvent.h,5);
            if(output.capacity>=8192-block)throw std::runtime_error("Monitor output buffer is too large");
            ComPtr<IAudioCaptureClient> capture;ComPtr<IAudioRenderClient> render;ComPtr<IAudioClockAdjustment> clock;
            if(input.client)check(input.client->GetService(IID_PPV_ARGS(&capture)),"Monitor capture");
            check(output.client->GetService(IID_PPV_ARGS(&render)),"Monitor render");
            check(output.client->GetService(IID_PPV_ARGS(&clock)),"Monitor clock");
            check(clock->SetSampleRate(rate),"Monitor sample rate");
            Ring<8192> queue;std::vector<float> mono(std::max(input.capacity,output.capacity));
            std::vector<RoutedSample> preview(output.capacity);
            auto queued=[&]{return effectsOnly?engine_.preview_.size():queue.size();};
            auto trim=[&](size_t keep){if(effectsOnly)engine_.preview_.trim(keep);else queue.trim(keep);};
            if(effectsOnly){engine_.preview_.trim(0);engine_.previewMask_=effectsMask;}
            BYTE* initial=nullptr;check(render->GetBuffer(output.capacity,&initial),"Monitor prime");
            check(render->ReleaseBuffer(output.capacity,AUDCLNT_BUFFERFLAGS_SILENT),"Monitor prime release");
            Mmcss priority;if(input.client)input.start();output.start();
            {std::lock_guard lock(mutex_);message_=outputName;}
            state=2;lost=0;bool primed=false;Drift drift;
            auto adjusted=std::chrono::steady_clock::now(),captured=adjusted;
            HANDLE events[]={stop_,captureEvent.h,renderEvent.h};
            while(engine_.running()) {
                const auto wait=WaitForMultipleObjects(3,events,FALSE,100);
                if(wait==WAIT_OBJECT_0)break;
                if(wait==WAIT_FAILED)throw std::runtime_error("Monitor audio wait failed");
                if(!effectsOnly && std::chrono::steady_clock::now()-captured>std::chrono::seconds(2))throw std::runtime_error("Виртуальный микрофон перестал подавать звук. Включите прослушивание повторно.");
                UINT32 n=0;if(capture)check(capture->GetNextPacketSize(&n),"Monitor packet");
                while(n) {
                    BYTE* bytes=nullptr;DWORD flags=0;
                    check(capture->GetBuffer(&bytes,&n,&flags,nullptr,nullptr),"Monitor capture buffer");
                    if(n>mono.size()){capture->ReleaseBuffer(n);throw std::runtime_error("Monitor packet exceeds allocation");}
                    if(flags&AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY){queue.trim(0);primed=false;}
                    const auto samples=reinterpret_cast<const float*>(bytes);
                    for(unsigned i=0;i<n;++i){
                        float v=0;
                        if(!(flags&AUDCLNT_BUFFERFLAGS_SILENT)) for(unsigned ch=0;ch<input.channels;++ch)v+=samples[i*input.channels+ch]/input.channels;
                        mono[i]=std::isfinite(v)?std::clamp(v,-1.0f,1.0f):0;
                    }
                    check(capture->ReleaseBuffer(n),"Monitor capture release");
                    if(!effectsOnly && !queue.push(mono.data(),n)){queue.trim(0);primed=false;}
                    captured=std::chrono::steady_clock::now();
                    check(capture->GetNextPacketSize(&n),"Monitor next packet");
                }
                UINT32 padding=0;check(output.client->GetCurrentPadding(&padding),"Monitor padding");
                if(padding>output.capacity)throw std::runtime_error("Monitor invalid padding");
                n=output.capacity-padding;
                if(n){
                    if(queued()>block+rate/20){trim(block);primed=false;drift={};}
                    if(!primed && queued()>=block+n)primed=true;
                    const bool have=primed && (effectsOnly?engine_.preview_.pop(preview.data(),n):queue.pop(mono.data(),n));
                    if(have && effectsOnly){
                        const auto epoch=engine_.effectEpoch.load();
                        const bool audible=!engine_.muted && engine_.stats.outputActive;
                        for(unsigned i=0;i<n;++i)mono[i]=previewSample(preview[i],effectsMask,epoch,audible);
                    }
                    if(!have)primed=false;
                    BYTE* dest=nullptr;check(render->GetBuffer(n,&dest),"Monitor render buffer");
                    if(have)for(unsigned i=0;i<n;++i)for(unsigned ch=0;ch<output.channels;++ch)reinterpret_cast<float*>(dest)[i*output.channels+ch]=mono[i];
                    check(render->ReleaseBuffer(n,have?0:AUDCLNT_BUFFERFLAGS_SILENT),"Monitor render release");
                    if(have){float previous=renderedPeak.load();const float now=peak(mono.data(),n);while(previous<now && !renderedPeak.compare_exchange_weak(previous,now)){} frames+=n;}
                }
                const auto now=std::chrono::steady_clock::now();
                if(primed && now-adjusted>=std::chrono::milliseconds(100)){
                    check(clock->SetSampleRate(static_cast<float>(rate*(1+drift.update(static_cast<double>(queued())-block)))),"Monitor drift");adjusted=now;
                }
            }
            break;
            } catch(const DeviceLost&) {
                if(++lost>20) throw;
                state=1;
                if(WaitForSingleObject(stop_,500)==WAIT_OBJECT_0 || !engine_.running()) break;
            }
            engine_.previewMask_=0;state=0;
        }catch(const std::exception& e){engine_.previewMask_=0;std::lock_guard lock(mutex_);message_=wide(e.what());state=3;}
    });}catch(...){state=0;throw;}
}
static void peakHold(std::atomic<float>& target,float value) {
    float previous=target.load();
    while(previous<value && !target.compare_exchange_weak(previous,value)) {}
}
Engine::Engine() {
    stop_=CreateEventW(nullptr,TRUE,FALSE,nullptr); data_=CreateEventW(nullptr,FALSE,FALSE,nullptr); ready_=CreateEventW(nullptr,TRUE,FALSE,nullptr);
    if(!stop_ || !data_ || !ready_) {
        if(stop_) CloseHandle(stop_); if(data_) CloseHandle(data_); if(ready_) CloseHandle(ready_);
        throw std::runtime_error("Cannot create audio synchronization events");
    }
}
Engine::~Engine() { stop(); CloseHandle(stop_); CloseHandle(data_); CloseHandle(ready_); }
void Engine::status(std::wstring s) { std::lock_guard lock(statusMutex_); status_=std::move(s); }
std::wstring Engine::status() const { std::lock_guard lock(statusMutex_); return status_; }
void Engine::fail(const std::exception& error) {
    FILETIME now{};GetSystemTimeAsFileTime(&now);uint64_t none=0;
    failedAt_.compare_exchange_strong(none,(static_cast<uint64_t>(now.dwHighDateTime)<<32)|now.dwLowDateTime);
    if(running_.exchange(false)) status(wide(error.what()));
    state=5; releaseEffects(); stats.pitchActive=false;stats.boostActive=false;stats.outputActive=false;stats.phraseState=0;stats.phraseSeconds=0;
    stats.rvcState=rvcEnabled?3:0;
    stats.inputPeak=0;stats.outputPeak=0;stats.noisePeak=0;SetEvent(stop_);
}
void Engine::reportError(const std::string& message) {status(wide(message)); state=5;releaseEffects();}
void Engine::start(const Config& c,uint64_t expectedOperation) {
    if(expectedOperation && operation!=expectedOperation)throw std::runtime_error("Audio operation cancelled");
    stop();
    if(c.input.empty() || c.output.empty() || (c.version!=1 && c.version!=2) || !std::isfinite(c.intensity) || c.intensity<0 || c.intensity>2 || c.bufferMs<10 || c.bufferMs>80 || c.periodMs<2 || c.periodMs>20 || c.cudaGraphs < -1 || c.cudaGraphs>1)
        throw std::runtime_error("Invalid audio settings");
    if(!c.tag) {
        std::wstring inputName,outputName;
        for(const auto& d:devices(true)) if(d.id==c.input) inputName=d.name;
        for(const auto& d:devices(false)) if(d.id==c.output) outputName=d.name;
        if(inputName.find(L"CABLE Output")!=std::wstring::npos && outputName.find(L"CABLE In")!=std::wstring::npos)
            throw std::runtime_error("Cannot route CABLE Output back to CABLE Input");
    }
    if(c.tag) {
        HANDLE owner=CreateMutexW(nullptr,FALSE,L"Local\\MicNoize.TAG");
        if(!owner) throw std::runtime_error("Cannot create TAG ownership mutex");
        if(GetLastError()==ERROR_ALREADY_EXISTS) {CloseHandle(owner);throw std::runtime_error("TAG is already running in another Mic Noize instance");}
        tagOwner_=owner;
        try {
            ensureTagHost();
            if(c.input==tagMicrophone().id)throw std::runtime_error("Select the physical microphone, not TAG's own output");
        } catch(...) {CloseHandle(tagOwner_);tagOwner_=nullptr;throw;}
    }
    captured_.reset(); cleaned_.reset(); desktop_.reset();resetEffect_=false;
    config_=c;
    ResetEvent(stop_); ResetEvent(data_); ResetEvent(ready_);
    stats.outputActive=false;
    stats.inputPeak=0; stats.outputPeak=0; stats.noisePeak=0; stats.processMs=0; stats.maxProcessMs=0; stats.reconfigureMs=0;
    stats.maxRunMs=0; stats.maxResetMs=0; stats.runsOver5Ms=0; stats.runsOver10Ms=0; stats.maxRunBlock=0; failedAt_=0;
    stats.inputQueue=0; stats.outputQueue=0; stats.renderPadding=0;
    stats.underruns=0; stats.drops=0; stats.discontinuities=0; stats.processed=0;
    stats.inputPeriodMs=0; stats.outputPeriodMs=0; stats.driftPpm=0;
    stats.tagBufferFrames=0; stats.tagDriverGaps=0; stats.tagFrames=0;
    stats.tagLateTicks=0; stats.tagMaxWakeMs=0; stats.tagReconnects=0;
    stats.pitchActive=false; stats.boostActive=false; stats.pitchDelayMs=0; stats.pitchMaxMs=0;stats.phraseState=0;stats.phraseSeconds=0;
    stats.rvcState=rvcEnabled?1:0;stats.rvcLatencyMs=0;stats.denoiser=0;
    intensity=c.intensity; releaseEffects(); running_=true; state=1; status(L"Loading NVIDIA model...");
    try {
        if(expectedOperation && operation!=expectedOperation)throw std::runtime_error("Audio operation cancelled");
        if(c.tag) {
            std::promise<float> ready;auto result=ready.get_future();
            tagLevelThread_=std::thread([this,ready=std::move(ready)]() mutable {
                bool initialized=false;
                try {
                    TagEndpointStatus endpoint;
                    auto read=[&] {
                        if(!readTagEndpointStatus(endpoint) || !endpoint.ready || !std::isfinite(endpoint.compensation) || endpoint.compensation<=0 || endpoint.compensation>1)
                            throw std::runtime_error(endpoint.error[0]?endpoint.error:"TAG endpoint controller unavailable");
                    };
                    read();const std::wstring identity=endpoint.endpoint;
                    ready.set_value(endpoint.compensation);initialized=true;
                    while(WaitForSingleObject(stop_,100)==WAIT_TIMEOUT) {
                        read();
                        if(identity!=endpoint.endpoint)throw std::runtime_error("TAG endpoint changed; reconnect processing");
                        tagLevelCompensation_=endpoint.compensation;
                    }
                } catch(const std::exception& error) {
                    if(initialized) fail(error);
                    else ready.set_exception(std::current_exception());
                }
            });
            tagLevelCompensation_=result.get();
            if(WaitForSingleObject(stop_,0)==WAIT_OBJECT_0) throw std::runtime_error("TAG level guard stopped");
        }
        dsp_=std::thread([this,c]{dspLoop(c);});
        io_=std::thread([this,c]{ioLoop(c);});
        desktopThread_=std::thread([this]{desktopLoop();});
        if(expectedOperation && operation!=expectedOperation)throw std::runtime_error("Audio operation cancelled");
    } catch(...) { stop(); throw; }
}
void Engine::stop() {
    const bool hadSession=io_.joinable() || dsp_.joinable();
    if(hadSession) state=4;
    releaseEffects();
    studioRecording=false;
    SetEvent(stop_);
    if(io_.joinable()) io_.join(); if(dsp_.joinable()) dsp_.join();if(desktopThread_.joinable())desktopThread_.join();if(tagLevelThread_.joinable())tagLevelThread_.join();
    if(tagOwner_) {CloseHandle(tagOwner_);tagOwner_=nullptr;}
    soundSeekRequest=0;soundPlaying=0;soundPosition=0;soundLength=0;
    if(running_.exchange(false)) status(L"Stopped");
    if(hadSession) {
        // Write only after audio threads have joined: no file I/O in the audio path.
        try {
            const auto folder=projectRoot()/L"results"; std::filesystem::create_directories(folder);
            std::ofstream log(folder/L"sessions.log",std::ios::app);
            auto format=[](const SYSTEMTIME& t){
                char text[32]; sprintf_s(text,"%04u-%02u-%02uT%02u:%02u:%02uZ",t.wYear,t.wMonth,t.wDay,t.wHour,t.wMinute,t.wSecond);
                return std::string(text);
            };
            SYSTEMTIME time{}; GetSystemTime(&time);
            log<<format(time)<<" output="<<(config_.tag?"TAG":"WASAPI")<<" version="<<config_.version<<" reserve_ms="<<config_.bufferMs
               <<" graphs="<<config_.cudaGraphs<<" blocks="<<stats.processed<<" underruns="<<stats.underruns<<" drops="<<stats.drops
               <<" run_max_ms="<<stats.maxRunMs<<" reset_max_ms="<<stats.maxResetMs<<" tag_gaps="<<stats.tagDriverGaps
               <<" late_ticks="<<stats.tagLateTicks<<" reconnects="<<stats.tagReconnects
               <<" denoiser="<<(stats.denoiser==1?"nvidia":stats.denoiser==2?"bypass":stats.denoiser==3?"cpu":stats.denoiser==4?"input":"none")<<" runs_over_5ms="<<stats.runsOver5Ms<<" runs_over_10ms="<<stats.runsOver10Ms<<" run_max_at_s="<<stats.maxRunBlock/100;
            if(const auto failed=failedAt_.load()) {
                // The line is written on the next stop(), often much later: keep when it really broke.
                const FILETIME file{static_cast<DWORD>(failed),static_cast<DWORD>(failed>>32)};SYSTEMTIME at{};
                if(FileTimeToSystemTime(&file,&at)) log<<" failed_at="<<format(at);
            }
            log<<" status="<<utf8(status())<<'\n';
            if(!log) OutputDebugStringW(L"Mic Noize: could not write results/sessions.log\n");
        } catch(...) {OutputDebugStringW(L"Mic Noize: session log unavailable\n");}
    }
    stats.outputActive=false;
    stats.inputPeak=0; stats.outputPeak=0; stats.noisePeak=0; stats.inputQueue=0; stats.outputQueue=0;
    stats.pitchActive=false; stats.boostActive=false; stats.phraseState=0;stats.phraseSeconds=0;state=0;
    stats.rvcState=0;stats.rvcLatencyMs=0;stats.denoiser=0;
    stats.desktopSource=false;stats.desktopState=0;
}
void Engine::dspLoop(Config c) {
    try {
        // Without a usable NVIDIA GPU or model the voice still goes out, only without denoising:
        // effects, the virtual microphone and the soundpad do not depend on NVIDIA.
        std::unique_ptr<Afx> fx;
        const CpuDenoiserApi* cpuApi=nullptr;
        struct Cpu {const CpuDenoiserApi*& api;void* state=nullptr;~Cpu(){if(state)api->destroy(state);}} cpu{cpuApi};
        // RTX Voice (it also runs on GTX 10xx) or NVIDIA Broadcast as the input already denoised
        // the voice on the GPU: a second denoiser would only add latency and artifacts.
        std::wstring cleanedBy;
        try {
            for(const auto& d:devices(true)) if(d.id==c.input) {
                auto lower=d.name;std::transform(lower.begin(),lower.end(),lower.begin(),[](wchar_t ch){return static_cast<wchar_t>(towlower(ch));});
                if(lower.find(L"rtx voice")!=std::wstring::npos || lower.find(L"nvidia broadcast")!=std::wstring::npos) cleanedBy=d.name;
            }
        } catch(...) {}
        if(!cleanedBy.empty()) {{std::lock_guard lock(statusMutex_);denoiserMessage_=cleanedBy;}stats.denoiser=4;}
        else try {
            // MNR_DENOISER=cpu tests the CPU path on an RTX machine (and frees the GPU for games).
            if(_wcsicmp(environment(L"MNR_DENOISER").c_str(),L"cpu")==0)
                throw std::runtime_error("выбран процессор (MNR_DENOISER=cpu)");
            fx=std::make_unique<Afx>(c);stats.denoiser=1;
        }
        catch(const std::exception& e) {
            // No usable NVIDIA: DeepFilterNet on the CPU when the host supplied it, else no denoiser.
            {std::lock_guard lock(statusMutex_);denoiserMessage_=wide(e.what());}
            cpuApi=cpuDenoiser.load();
            // Flush denormals on this thread: DeepFilterNet's recurrent state decays into them on
            // quiet input, and older CPUs take a large penalty per denormal operation.
            if(cpuApi) {_mm_setcsr(_mm_getcsr()|0x8040);cpu.state=cpuApi->create();}
            stats.denoiser=cpu.state?3:2;
        }
        PitchEffect pitchEffect; AutoTunePitch autoTune; StutterEffect stutterMic,stutterDiscord;
        EchoEffect echoMic,echoDiscord; PhraseEffect phraseEffect; NoiseGate noiseGate;
        auto rvc=std::make_unique<RvcClient>(stats,rvcConfig); Mmcss priority;
        std::array<float,block> in{},out{},microphone{},microphoneRaw{},gateEnvelope{},discordSource{},scratch{},echoMicOut{},echoDiscordOut{},captureData{};
        LastEffect lastEffect;OutputEffects boostEffect;SoundPlayer sounds;
        bool clipPending=false,studioActive=false,studioPending=false;
        {std::lock_guard lock(clipMutex_);clip_.clear();clip_.reserve(48000*20);} // publishing never allocates
        std::vector<float> studioBuffer;studioBuffer.reserve(rate*20);
        {std::lock_guard lock(studioMutex_);studioClip_.reserve(rate*20);}
        std::array<float,block> sound{};
        std::array<RoutedSample,block> routed{};
        std::array<uint8_t,block> modified{},recording{},captureFlags{};
        std::array<float,block+1> discord{};SourceRouting routing;Drift discordDrift;
        bool discordPrimed=false,wasDiscord=false;Ramp sourceFade{1};
        float applied=c.intensity;
        // Build lazy CUDA/graph state before opening the microphone or output stream.
        for(unsigned j=0;j<block;++j) in[j]=0.02f*std::sin(j*0.07f)+0.01f*std::sin(j*0.21f);
        if(fx) {
            for(unsigned i=0;i<20;++i) { if(WaitForSingleObject(stop_,0)==WAIT_OBJECT_0) return; fx->process(in.data(),out.data()); }
            fx->reset();
        }
        soundRequest=0;soundSeekRequest=0;soundPlaying=0;soundPosition=0;soundLength=0; // a press before start never plays later
        SetEvent(ready_);
        HANDLE events[]={stop_,data_};
        while(WaitForMultipleObjects(2,events,FALSE,INFINITE)==WAIT_OBJECT_0+1) {
            while(WaitForSingleObject(stop_,0)!=WAIT_OBJECT_0) {
                // Do not discard audio that can still fit within the selected output reserve.
                const unsigned inputBudget=std::max(block*4,c.bufferMs*48+block*2);
                if(captured_.size()>inputBudget) { captured_.trim(block*2); resetEffect_=true; ++stats.drops; }
                if(!captured_.pop(in.data(),block)) break;
                auto begin=std::chrono::steady_clock::now();
                if(resetEffect_.exchange(false) && fx) {
                    fx->reset();
                    stats.maxResetMs=std::max(stats.maxResetMs.load(),std::chrono::duration<float,std::milli>(std::chrono::steady_clock::now()-begin).count());
                }
                const float wanted=heldIntensity(intensity.load(),alternateIntensity.load(),noiseHeldSample.load(),
                    effectEpoch.load(),GetTickCount64(),running_ && stats.outputActive && !muted);
                if(wanted!=applied && fx) {
                    auto start=std::chrono::steady_clock::now(); fx->strength(wanted); applied=wanted;
                    stats.reconfigureMs=std::max(stats.reconfigureMs.load(),std::chrono::duration<float,std::milli>(std::chrono::steady_clock::now()-start).count());
                }
                const auto runBegin=std::chrono::steady_clock::now();
                if(fx) fx->process(in.data(),out.data());
                else if(cpu.state) {if(!cpuApi->process(cpu.state,in.data(),out.data(),wanted)) throw std::runtime_error("DeepFilterNet failed");}
                else out=in;
                const float runMs=std::chrono::duration<float,std::milli>(std::chrono::steady_clock::now()-runBegin).count();
                if(runMs>stats.maxRunMs.load(std::memory_order_relaxed)){stats.maxRunMs=runMs;stats.maxRunBlock=stats.processed.load(std::memory_order_relaxed);}
                if(runMs>5) stats.runsOver5Ms.fetch_add(1,std::memory_order_relaxed);
                if(runMs>10) stats.runsOver10Ms.fetch_add(1,std::memory_order_relaxed);
                float ms=std::chrono::duration<float,std::milli>(std::chrono::steady_clock::now()-begin).count();
                stats.processMs=ms; stats.maxProcessMs=std::max(stats.maxProcessMs.load(),ms);
                for(float v:out) if(!std::isfinite(v)) throw std::runtime_error("NVIDIA returned a non-finite audio sample");
                for(float& v:out) v=std::clamp(v,-1.0f,1.0f);
                // RVC belongs to the microphone path only: it runs before the Discord source switch, so
                // the converted voice also feeds the background mix while a Discord effect is held.
                modified.fill(0);
                rvc->process(out.data(),block,rvcEnabled.load() && !muted,modified.data());
                // Keep raw input for effects and a gated copy for live/background voice.
                // RVC is still voice: its monitor marks must not bypass the gate.
                microphoneRaw=out;microphone=out;
                for(auto& mark:modified)if(mark)mark=ModifiedVoice;
                peakHold(stats.noisePeak,noiseGate.process(microphone.data(),block,noiseGateDb.load(),gateEnvelope.data()));
                // Capture processed microphone before Discord source selection. Publication is a
                // try-lock swap of buffers reserved before this real-time loop.
                if(studioRecording && !studioActive && !studioPending && !muted) studioActive=true;
                if(studioActive) {
                    if(!studioRecording || muted) {
                        studioRecording=false;studioActive=false;studioPending=!studioBuffer.empty();
                    } else {
                        studioBuffer.insert(studioBuffer.end(),microphone.begin(),microphone.end());
                        if(studioBuffer.size()>=rate*20) {
                            studioRecording=false;studioActive=false;studioPending=true;
                        }
                    }
                }
                if(studioPending) {
                    if(std::unique_lock lock(studioMutex_,std::try_to_lock);lock.owns_lock()) {
                        studioClip_.swap(studioBuffer);studioBuffer.clear();++studioGeneration;studioPending=false;
                    }
                }
                const auto hold=heldSample.load();const auto epoch=effectEpoch.load();
                bool valid=heldFresh(hold,epoch,GetTickCount64()) && running_ && stats.outputActive && !muted;
                const unsigned flags=valid?static_cast<unsigned>(hold&HoldAllMask):0;
                bool fromDiscord=routing.select(flags,phraseEffect.state()!=0);
                if(desktop_.size()>block*6){desktop_.trim(block*2);discordDrift={};}
                if(stats.desktopState!=2){desktop_.trim(0);discordPrimed=false;echoDiscord.reset();}
                if(!discordPrimed && desktop_.size()>=block*2)discordPrimed=true;
                const auto take=static_cast<unsigned>(std::clamp(std::lround(block*(1+discordDrift.update(static_cast<double>(desktop_.size())-block*2))),static_cast<long>(block-1),static_cast<long>(block+1)));
                const bool haveDiscord=discordPrimed && desktop_.pop(discord.data(),take);
                if(!haveDiscord)discordPrimed=false;
                for(unsigned i=0;i<block;++i){
                    const double at=i*static_cast<double>(take-1)/(block-1);const auto j=static_cast<unsigned>(at);
                    discordSource[i]=haveDiscord?std::lerp(discord[j],discord[std::min(j+1,take-1)],static_cast<float>(at-j)):0;
                }
                if(fromDiscord){
                    out=discordSource;
                    valid=valid && stats.desktopState==2;
                    modified.fill(0); // Converted microphone now travels separately.
                }
                const unsigned selected=sourceHeld(flags,fromDiscord);
                if(fromDiscord!=wasDiscord){pitchEffect.reset();autoTune.reset();boostEffect=OutputEffects{};sourceFade=Ramp{0};wasDiscord=fromDiscord;}
                stats.desktopSource=fromDiscord;
                if(valid){
                    (fromDiscord?stutterDiscord:stutterMic).process(out.data(),block,(selected&HoldStutter)!=0,stutterMs.load(),modified.data());
                    scratch.fill(0);
                    (fromDiscord?stutterMic:stutterDiscord).process(scratch.data(),block,false,stutterMs.load());
                }else{scratch.fill(0);stutterMic.process(scratch.data(),block,false,stutterMs.load());stutterDiscord.process(scratch.data(),block,false,stutterMs.load());}
                stutterMic.feed(microphoneRaw.data(),block);
                if(haveDiscord)stutterDiscord.feed(discordSource.data(),block);else stutterDiscord.reset();
                const auto pitchBegin=std::chrono::steady_clock::now();
                const bool pitchHeld=valid && (selected&HoldPitch)!=0;
                const bool tuneHeld=valid && (selected&HoldAutoTune)!=0;
                const int manual=pitchHeld?pitch.load():0;
                const float correction=autoTune.process(out.data(),block,tuneHeld,manual,tuneRoot.load(),tuneScale.load(),tuneSpeedMs.load(),tuneStrength.load());
                const int formants=pitchHeld?formant.load():0;
                pitchEffect.processAdvanced(out.data(),block,static_cast<float>(manual)+correction,static_cast<float>(formants),(pitchHeld&&(manual||formants))||tuneHeld,modified.data());
                for(auto& v:out)v*=sourceFade.next(1);
                stats.pitchActive=pitchEffect.active(); stats.pitchDelayMs=pitchEffect.delayMs();
                stats.pitchMaxMs=std::max(stats.pitchMaxMs.load(),std::chrono::duration<float,std::milli>(std::chrono::steady_clock::now()-pitchBegin).count());
                const bool phraseWasActive=phraseEffect.state()!=0;
                phraseEffect.process(out.data(),block,routing.phraseFlags,slowSpeed.load(),fastSpeed.load(),valid,epoch,phraseCancel.load(),!fromDiscord,modified.data());
                stats.phraseState=phraseEffect.state();stats.phraseSeconds=phraseEffect.seconds();
                const bool boosted=valid && (selected&HoldBoost)!=0;
                boostEffect.process(out.data(),block,1,boost.load(),boosted,overload.load(),nullptr,1,modified.data());
                stats.boostActive=boosted && boost>1;
                const bool micEchoBefore=echoMic.active(),discordEchoBefore=echoDiscord.active();
                const bool echoHeld=valid && (selected&HoldEcho)!=0;
                if(!valid){echoMic.reset();echoDiscord.reset();}
                echoMic.process(fromDiscord?nullptr:out.data(),block,echoHeld&&!fromDiscord,echoDelayMs.load(),echoRepeats.load(),echoDecay.load(),echoLevel.load(),echoMicOut.data());
                echoDiscord.process(fromDiscord?out.data():nullptr,block,echoHeld&&fromDiscord,echoDelayMs.load(),echoRepeats.load(),echoDecay.load(),echoLevel.load(),echoDiscordOut.data());
                const bool micEcho=micEchoBefore||echoMic.active(),discordEcho=discordEchoBefore||echoDiscord.active();
                if(echoHeld){out.fill(0);modified.fill(0);}
                NoiseGate::applyVoice(out.data(),block,gateEnvelope.data(),modified.data(),fromDiscord);
                const bool recordDiscord=(selected&LastEffect::recordable)?fromDiscord:lastEffect.recordingDiscord();
                const bool chosenEcho=recordDiscord?discordEcho:micEcho;
                const bool captureMain=(selected&LastEffect::recordable)!=0 || phraseWasActive || phraseEffect.state()!=0;
                for(unsigned i=0;i<block;++i){
                    captureFlags[i]=static_cast<uint8_t>((captureMain?(modified[i]&~ModifiedBoost):0)|(chosenEcho?ModifiedEffects:0));
                    captureData[i]=((captureMain&&(modified[i]&~ModifiedBoost))?out[i]:0)+(recordDiscord?echoDiscordOut[i]:echoMicOut[i]);
                }
                effectActivity=sourceRecordFlags((pitchHeld?HoldPitch:0)|
                    (valid?selected&(HoldStutter|HoldAutoTune):0),fromDiscord)|
                    (micEcho?HoldEcho:0)|(discordEcho?HoldEcho<<4:0);
                const bool replay=lastEffect.process(out.data(),block,modified.data(),fromDiscord,sourceRecordFlags(selected,fromDiscord),
                    phraseWasActive || phraseEffect.state()!=0 || pitchEffect.active(),valid,epoch,phraseCancel.load(),replayRequest.load(),
                    captureData.data(),captureFlags.data(),chosenEcho);
                // Hand a finished recording to the UI without allocating or waiting here.
                if(lastEffect.finished())clipPending=true;
                // A new hold already replaced the slot: that recording is gone, never publish half of it.
                if(lastEffect.capturing())clipPending=false;
                if(clipPending && lastEffect.count()){
                    if(std::unique_lock lock(clipMutex_,std::try_to_lock);lock.owns_lock()){
                        lastEffect.copyRecording(clip_,discordVolume.load());
                        ++clipGeneration;clipPending=false;
                    }
                }
                // Soundpad: the library lock is only tried; a contended block retries the same request next time.
                if(const auto request=soundRequest.load();request){
                    if(const unsigned id=sounds.request(request)){
                        if(std::unique_lock lock(soundMutex_,std::try_to_lock);lock.owns_lock()){
                            const auto found=sounds_.find(id);sounds.commit(request);
                            sounds.start(id,found==sounds_.end()?nullptr:found->second);
                        }
                    }
                }
                if(auto seek=soundSeekRequest.load();seek && sounds.playing()==unsigned(seek>>32)
                    && sounds.seek(unsigned(seek>>32),unsigned(seek))){
                    soundSeekRequest.compare_exchange_strong(seek,0);
                }
                sounds.render(sound.data(),block,soundVolume.load(),muted,recording.data());
                soundPlaying=sounds.playing();soundPosition=sounds.position();soundLength=sounds.length();
                for(unsigned i=0;i<block;++i)routed[i]={out[i],static_cast<uint8_t>(fromDiscord),modified[i],epoch,((fromDiscord&&!echoHeld)||replay)?microphone[i]:0,sound[i],recording[i],echoMicOut[i],echoDiscordOut[i]};
                if(!cleaned_.push(routed.data(),block)) ++stats.drops;
                ++stats.processed;
                stats.inputQueue=static_cast<unsigned>(captured_.size());
            }
        }
        // Preserve a recording when processing stops before the next audio block publishes it.
        if(!studioBuffer.empty()) {
            std::lock_guard lock(studioMutex_);
            studioClip_.swap(studioBuffer);
            ++studioGeneration;
        }
    } catch(const std::exception& e) { fail(e); }
}
void Engine::preview(const float* audio,const float* echo,const RoutedSample* routed,const uint8_t* modified,unsigned count) {
    const auto mask=previewMask_.load();
    if(!mask)return;
    const auto epoch=effectEpoch.load();
    const bool audible=!muted && stats.outputActive;
    std::array<RoutedSample,block> samples{};
    for(unsigned offset=0;offset<count;offset+=block){
        const auto n=std::min(block,count-offset);
        for(unsigned i=0;i<n;++i){
            const auto at=offset+i;
            samples[i]=previewQueued(audio[at],modified[at],routed[at].epoch,routed[at].sound,routed[at].recording,mask,epoch,audible);
            if((mask&ModifiedEffects) && audible && routed[at].epoch==epoch && echo[at]!=0){
                samples[i].value+=echo[at];samples[i].modified|=ModifiedEffects;
            }
        }
        // Preview must never block or trim from the producer side.
        if(!preview_.push(samples.data(),n))break;
    }
}
// Under soundMutex_: frees retired clips no player holds. Removed from the library, a clip
// cannot gain a new holder, so a count of one is final.
static void releaseSounds(std::vector<std::shared_ptr<const SoundClip>>& retired) {
    std::erase_if(retired,[](const auto& clip){return clip.use_count()==1;});
}
void Engine::soundLoad(unsigned id,std::vector<float> samples,float gain,unsigned loopStart,unsigned loopEnd,bool independentVolume) {
    auto clip=std::make_shared<SoundClip>();clip->samples=std::move(samples);clip->gain=gain;
    clip->loopRange=(uint64_t(loopStart)<<32)|loopEnd;clip->independentVolume=independentVolume;
    std::lock_guard lock(soundMutex_);auto& slot=sounds_[id];
    if(slot)retiredSounds_.push_back(std::move(slot));
    slot=std::move(clip);releaseSounds(retiredSounds_);
}
bool Engine::soundGain(unsigned id,float gain) {
    std::lock_guard lock(soundMutex_);const auto found=sounds_.find(id);
    if(found==sounds_.end())return false;found->second->gain=gain;return true;
}
void Engine::soundClear() {
    std::lock_guard lock(soundMutex_);
    for(auto& entry:sounds_)retiredSounds_.push_back(std::move(entry.second));
    sounds_.clear();releaseSounds(retiredSounds_);soundPlay(0);
}
std::shared_ptr<const SoundClip> Engine::soundClip(unsigned id) {
    std::lock_guard lock(soundMutex_);const auto found=sounds_.find(id);return found==sounds_.end()?nullptr:found->second;
}
unsigned Engine::clipCopy(float* out,unsigned capacity,unsigned* generation) {
    std::lock_guard lock(clipMutex_);
    if(generation)*generation=clipGeneration.load();
    const auto count=static_cast<unsigned>(clip_.size());
    if(out&&capacity)std::copy_n(clip_.begin(),std::min(count,capacity),out);
    return count;
}
bool Engine::soundLoop(unsigned id,unsigned start,unsigned end) {
    std::lock_guard lock(soundMutex_);const auto found=sounds_.find(id);
    if(found==sounds_.end() || (!end && start) || (end && (start>=end || end>found->second->samples.size())))return false;
    found->second->loopRange=(uint64_t(start)<<32)|end;return true;
}
unsigned Engine::studioCopy(float* out,unsigned capacity,unsigned* generation) {
    std::lock_guard lock(studioMutex_);
    if(generation)*generation=studioGeneration.load();
    const auto count=static_cast<unsigned>(studioClip_.size());
    if(out&&capacity)std::copy_n(studioClip_.begin(),std::min(count,capacity),out);
    return count;
}
// Called from the hotkey thread and the UI; a quick second press of the same clip restarts it.
void Engine::soundPlay(unsigned id,bool forceRestart) {
    std::lock_guard lock(soundRequestMutex_);
    const auto now=GetTickCount64();
    const bool restart=forceRestart || (id && id==soundPressId_ && now-soundPressTick_<soundDoublePressMs);
    soundPressId_=id;soundPressTick_=now;
    soundSeekRequest=0;
    soundRequest=SoundPlayer::pack(id,++soundSerial_,restart);
}
void Engine::soundSeek(unsigned id,unsigned frame) {
    soundSeekRequest=(uint64_t(id)<<32)|frame;
}
void Engine::tagLoop(Config c) {
    Event captureEvent;
    Stream input; input.open(c.input,true,captureEvent.h,c.periodMs);
    TagClient tag;
    ComPtr<IAudioCaptureClient> capture;
    check(input.client->GetService(IID_PPV_ARGS(&capture)),"TAG microphone capture client");
    std::vector<float> mono(input.capacity+block);
    // Allocate once before rendering: these buffers exceed the audio thread's stack budget.
    constexpr unsigned capacity=16384;
    std::vector<float> output(capacity);
    std::vector<RoutedSample> routed(capacity);
    std::vector<uint8_t> sources(capacity),modified(capacity);
    std::vector<float> microphone(capacity),effectOnly(capacity),sound(capacity),echoMic(capacity),echoDiscord(capacity),echoOnly(capacity);
    HANDLE timer=CreateWaitableTimerExW(nullptr,nullptr,CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,TIMER_ALL_ACCESS);
    if(!timer) throw std::runtime_error("TAG high-resolution timer creation failed");
    struct Timer {HANDLE h; ~Timer(){CancelWaitableTimer(h);CloseHandle(h);}} closeTimer{timer};
    LARGE_INTEGER due{}; due.QuadPart=-20000;
    if(!SetWaitableTimer(timer,&due,2,nullptr,nullptr,FALSE)) throw std::runtime_error("TAG timer start failed");
    Mmcss priority; input.start(); stats.inputPeriodMs=input.periodMs; stats.outputPeriodMs=2;
    status(L"TAG ready; select its microphone in a receiving application");
    state=2;
    const unsigned target=c.bufferMs*48;
    bool wasRunning=false,primed=false; float fade=0;
    OutputEffects effects;
    double correction=0; Drift drift; TagClock clock;
    auto last=std::chrono::steady_clock::now(),lastCapture=last,lastAdjustment=last;
    HANDLE events[]={stop_,captureEvent.h,timer};
    for(;;) {
        DWORD wait=WaitForMultipleObjects(3,events,FALSE,2000);
        if(wait==WAIT_OBJECT_0) return;
        if(wait==WAIT_FAILED || wait==WAIT_TIMEOUT) throw std::runtime_error("TAG audio event wait failed");
        if(testStallMs.load(std::memory_order_relaxed)) if(const auto ms=testStallMs.exchange(0)) Sleep(ms);
        tag.handleEvent();
        UINT32 n=0; check(capture->GetNextPacketSize(&n),"TAG capture packet size");
        while(n) {
            BYTE* bytes=nullptr; DWORD flags=0;
            check(capture->GetBuffer(&bytes,&n,&flags,nullptr,nullptr),"TAG capture buffer");
            if(n>mono.size()){capture->ReleaseBuffer(n);throw std::runtime_error("TAG capture packet exceeds allocation");}
            if(flags&AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY){++stats.discontinuities;resetEffect_=true;}
            const auto samples=reinterpret_cast<const float*>(bytes);
            for(unsigned i=0;i<n;++i) {
                float v=0;
                if(!(flags&AUDCLNT_BUFFERFLAGS_SILENT)) for(unsigned ch=0;ch<input.channels;++ch) v+=samples[i*input.channels+ch]/input.channels;
                mono[i]=std::isfinite(v)?std::clamp(v,-1.0f,1.0f):0;
            }
            check(capture->ReleaseBuffer(n),"TAG release capture");
            peakHold(stats.inputPeak,peak(mono.data(),n));
            if(!captured_.push(mono.data(),n)){++stats.drops;resetEffect_=true;}
            SetEvent(data_); lastCapture=std::chrono::steady_clock::now();
            check(capture->GetNextPacketSize(&n),"TAG next capture packet");
        }
        const auto now=std::chrono::steady_clock::now();
        if(now-lastCapture>std::chrono::seconds(2)) throw std::runtime_error("Microphone stopped delivering audio");
        const bool running=tag.running();
        if(running!=wasRunning) {releaseEffects();cleaned_.trim(0);state=running?3:2;}
        stats.outputActive=running;
        if(!running || !wasRunning) {
            stats.outputPeak=0;
            cleaned_.trim(target); primed=false; fade=0; clock={}; drift={}; correction=0;
            lastAdjustment=now;
            if(running!=wasRunning) status(running?L"Running: NVIDIA v"+std::to_wstring(c.version)+L" -> TAG":L"TAG ready; waiting for recording client");
        } else {
            // TAG's device clock belongs to the host. Follow USB clock drift by gently
            // adjusting notification timing; PCM itself is not resampled here.
            const unsigned maxFrames=std::min(tag.capacity()/2,static_cast<unsigned>(output.size()));
            const double elapsed=std::chrono::duration<double>(now-last).count();
            stats.tagMaxWakeMs=std::max(stats.tagMaxWakeMs.load(),static_cast<float>(elapsed*1000));
            if(elapsed*rate>maxFrames) ++stats.tagLateTicks;
            // Keep overdue frames for the next tick. A late wake must neither lose
            // device-clock time nor discard/re-prime already processed audio.
            const unsigned frames=clock.take(elapsed,correction,maxFrames);
            if(frames) {
                if(cleaned_.size()>target+rate/20){cleaned_.trim(target);++stats.drops;fade=0;drift={};correction=0;}
                if(!primed && cleaned_.size()>=target+frames) primed=true;
                const bool have=primed && cleaned_.pop(routed.data(),frames);
                if(primed && !have){++stats.underruns;primed=false;fade=0;drift={};correction=0;}
                if(!have)std::fill_n(routed.data(),frames,RoutedSample{});
                const auto currentEpoch=effectEpoch.load();
                for(unsigned i=0;i<frames;++i){output[i]=have?routed[i].value:0;sources[i]=have?routed[i].discord:0;modified[i]=have?routed[i].modified:0;microphone[i]=have?routed[i].microphone:0;sound[i]=have?routed[i].sound:0;echoMic[i]=have&&routed[i].epoch==currentEpoch?routed[i].echoMic:0;echoDiscord[i]=have&&routed[i].epoch==currentEpoch?routed[i].echoDiscord:0;}
                effects.process(output.data(),frames,volume.load(),1,false,false,sources.data(),discordVolume.load(),modified.data(),microphone.data(),effectOnly.data(),sound.data(),echoMic.data(),echoDiscord.data(),echoOnly.data());
                for(unsigned i=0;i<frames;++i) {
                    fade+=std::clamp((muted?0.0f:1.0f)-fade,-1.0f/240,1.0f/240);
                    output[i]*=fade;
                    effectOnly[i]*=fade;
                    echoOnly[i]*=fade;
                }
                const float outputPeak=peak(output.data(),frames);
                // Windows software endpoint gain applies to shared-mode capture only.
                const float compensation=tagLevelCompensation_.load();
                for(unsigned i=0;i<frames;++i) output[i]*=compensation;
                if(tag.write(output.data(),frames)) {
                    stats.tagFrames+=frames;
                    preview(effectOnly.data(),echoOnly.data(),routed.data(),modified.data(),frames);
                    if(muted && fade==0) stats.outputPeak=0;
                    else peakHold(stats.outputPeak,outputPeak);
                } else {
                    // The host dropped us during a stall and filled it with silence: discard the
                    // clock debt and re-prime exactly as on a client transition.
                    ++stats.tagReconnects;
                    releaseEffects();cleaned_.trim(0);primed=false;fade=0;clock={};drift={};correction=0;
                    lastAdjustment=now;
                }
            }
            if(primed && now-lastAdjustment>=std::chrono::milliseconds(100)) {
                correction=drift.update(static_cast<double>(cleaned_.size())-target);
                stats.driftPpm=static_cast<float>(correction*1e6); lastAdjustment=now;
            }
        }
        stats.outputQueue=static_cast<unsigned>(cleaned_.size());
        stats.tagBufferFrames=tag.capacity(); stats.tagDriverGaps=tag.driverGaps();
        last=now; wasRunning=running;
    }
}
void Engine::ioLoop(Config c) {
    try {
        Com com;
        HANDLE readyEvents[]={stop_,ready_};
        if(WaitForMultipleObjects(2,readyEvents,FALSE,INFINITE)!=WAIT_OBJECT_0+1) return;
        if(c.tag) { tagLoop(c); return; }
        Event captureEvent,renderEvent;
        Stream input,output;
        input.open(c.input,true,captureEvent.h,c.periodMs);
        output.open(c.output,false,renderEvent.h,c.periodMs);
        ComPtr<IAudioCaptureClient> capture; ComPtr<IAudioRenderClient> render; ComPtr<IAudioClockAdjustment> clock;
        check(input.client->GetService(IID_PPV_ARGS(&capture)),"Capture client");
        check(output.client->GetService(IID_PPV_ARGS(&render)),"Render client");
        check(output.client->GetService(IID_PPV_ARGS(&clock)),"Clock drift correction");
        check(clock->SetSampleRate(rate),"Set output sample rate");
        stats.inputPeriodMs=input.periodMs; stats.outputPeriodMs=output.periodMs;
        std::vector<float> mono(std::max(input.capacity,output.capacity)+block);
        std::vector<RoutedSample> routed(output.capacity);
        std::vector<uint8_t> sources(output.capacity);
        std::vector<uint8_t> modified(output.capacity);
        std::vector<float> microphone(output.capacity),effectOnly(output.capacity),sound(output.capacity),echoMic(output.capacity),echoDiscord(output.capacity),echoOnly(output.capacity);
        BYTE* initial=nullptr; check(render->GetBuffer(output.capacity,&initial),"Prime render buffer");
        check(render->ReleaseBuffer(output.capacity,AUDCLNT_BUFFERFLAGS_SILENT),"Prime render buffer release");
        Mmcss priority;
        output.start(); input.start(); stats.outputActive=true; state=3; status(L"Running");
        unsigned target=c.bufferMs*48;
        bool primed=false; float fade=0;
        OutputEffects effects;
        Drift drift;
        auto lastAdjustment=std::chrono::steady_clock::now(),lastCapture=lastAdjustment;
        HANDLE events[]={stop_,captureEvent.h,renderEvent.h};
        for(;;) {
            DWORD wait=WaitForMultipleObjects(3,events,FALSE,2000);
            if(wait==WAIT_OBJECT_0) break;
            if(wait==WAIT_TIMEOUT) throw std::runtime_error("Audio device stopped delivering events; reconnect and restart");
            if(wait==WAIT_FAILED) throw std::runtime_error("Audio event wait failed");
            UINT32 available=0; check(capture->GetNextPacketSize(&available),"Capture packet size");
            while(available) {
                BYTE* data=nullptr; UINT32 n=0; DWORD flags=0;
                check(capture->GetBuffer(&data,&n,&flags,nullptr,nullptr),"Capture buffer");
                if(n>mono.size()) { capture->ReleaseBuffer(n); throw std::runtime_error("Capture packet exceeds allocated capacity"); }
                if(flags&AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY) { ++stats.discontinuities; resetEffect_=true; }
                auto f=reinterpret_cast<const float*>(data);
                for(UINT32 i=0;i<n;++i) {
                    // QuadCast exposes two channels. Average rather than amplify correlated stereo.
                    float value=0;
                    if(!(flags&AUDCLNT_BUFFERFLAGS_SILENT)) for(unsigned ch=0;ch<input.channels;++ch) value+=f[i*input.channels+ch]/input.channels;
                    mono[i]=std::isfinite(value)?std::clamp(value,-1.0f,1.0f):0;
                }
                check(capture->ReleaseBuffer(n),"Release capture buffer");
                peakHold(stats.inputPeak,peak(mono.data(),n));
                if(!captured_.push(mono.data(),n)) { ++stats.drops; resetEffect_=true; }
                SetEvent(data_); lastCapture=std::chrono::steady_clock::now();
                check(capture->GetNextPacketSize(&available),"Next capture packet");
            }
            if(std::chrono::steady_clock::now()-lastCapture>std::chrono::seconds(2))
                throw std::runtime_error("Microphone stopped delivering audio; reconnect and restart");
            UINT32 padding=0; check(output.client->GetCurrentPadding(&padding),"Output padding");
            stats.renderPadding=padding;
            unsigned n=output.capacity-padding;
            if(n) {
                if(cleaned_.size()>target+rate/20) {
                    cleaned_.trim(target); ++stats.drops; fade=0; drift={};
                    check(clock->SetSampleRate(rate),"Reset output clock"); stats.driftPpm=0;
                }
                if(!primed && cleaned_.size()>=target+n) primed=true;
                bool have=primed && cleaned_.pop(routed.data(),n);
                if(primed && !have) {
                    ++stats.underruns; primed=false; fade=0; drift={};
                    check(clock->SetSampleRate(rate),"Reset output clock"); stats.driftPpm=0;
                }
                BYTE* dest=nullptr; check(render->GetBuffer(n,&dest),"Render buffer");
                float outputPeak=0;
                if(have) {
                    const auto currentEpoch=effectEpoch.load();
                    for(unsigned i=0;i<n;++i){mono[i]=routed[i].value;sources[i]=routed[i].discord;modified[i]=routed[i].modified;microphone[i]=routed[i].microphone;sound[i]=routed[i].sound;echoMic[i]=routed[i].epoch==currentEpoch?routed[i].echoMic:0;echoDiscord[i]=routed[i].epoch==currentEpoch?routed[i].echoDiscord:0;}
                    effects.process(mono.data(),n,volume.load(),1,false,false,sources.data(),discordVolume.load(),modified.data(),microphone.data(),effectOnly.data(),sound.data(),echoMic.data(),echoDiscord.data(),echoOnly.data());
                    auto out=reinterpret_cast<float*>(dest);
                    for(unsigned i=0;i<n;++i) {
                        float goal=muted?0.0f:1.0f;
                        fade+=std::clamp(goal-fade,-1.0f/240,1.0f/240);
                        const float value=mono[i]*fade;
                        mono[i]=value;
                        effectOnly[i]*=fade;
                        echoOnly[i]*=fade;
                        outputPeak=std::max(outputPeak,std::abs(value));
                        for(unsigned ch=0;ch<output.channels;++ch) out[i*output.channels+ch]=value;
                    }
                }
                check(render->ReleaseBuffer(n,have?0:AUDCLNT_BUFFERFLAGS_SILENT),"Release render buffer");
                if(!have){std::fill_n(mono.data(),n,0.0f);std::fill_n(modified.data(),n,uint8_t{0});std::fill_n(effectOnly.data(),n,0.0f);std::fill_n(echoOnly.data(),n,0.0f);std::fill_n(routed.data(),n,RoutedSample{});}
                preview(effectOnly.data(),echoOnly.data(),routed.data(),modified.data(),n);
                if(muted && fade==0) stats.outputPeak=0;
                else peakHold(stats.outputPeak,outputPeak);
            }
            auto now=std::chrono::steady_clock::now();
            if(primed && now-lastAdjustment>=std::chrono::milliseconds(100)) {
                // Observe the post-fill queue consistently; correction uses Windows' high-quality SRC.
                double correction=drift.update(static_cast<double>(cleaned_.size())-target);
                check(clock->SetSampleRate(static_cast<float>(rate*(1+correction))),"Adjust output clock");
                stats.driftPpm=static_cast<float>(correction*1e6); lastAdjustment=now;
            }
            stats.outputQueue=static_cast<unsigned>(cleaned_.size());
        }
    } catch(const std::exception& e) { fail(e); }
}

Headphones::Headphones() {
    stop_=CreateEventW(nullptr,TRUE,FALSE,nullptr);
    data_=CreateEventW(nullptr,FALSE,FALSE,nullptr);
    ready_=CreateEventW(nullptr,TRUE,FALSE,nullptr);
    if(!stop_ || !data_ || !ready_) {
        if(stop_)CloseHandle(stop_);if(data_)CloseHandle(data_);if(ready_)CloseHandle(ready_);
        throw std::runtime_error("Headphone events failed");
    }
}
Headphones::~Headphones(){stop();CloseHandle(stop_);CloseHandle(data_);CloseHandle(ready_);}
std::wstring Headphones::message() const {std::lock_guard lock(mutex_);return message_;}
void Headphones::fail(const std::exception& e) {
    {std::lock_guard lock(mutex_);message_=wide(e.what());}
    state=3;SetEvent(stop_);SetEvent(data_);
}
void Headphones::stop() {
    SetEvent(stop_);SetEvent(data_);
    if(io_.joinable())io_.join();if(dsp_.joinable())dsp_.join();
    if(owner_){CloseHandle(owner_);owner_=nullptr;}
    captured_.reset();cleaned_.reset();state=0;
}
void Headphones::start(const std::wstring& output,bool denoise) {
    stop();
    auto list=devices(false);
    auto found=std::find_if(list.begin(),list.end(),[&](const Device& d){return d.id==output;});
    if(found==list.end())throw std::runtime_error("Выберите подключённые физические наушники.");
    if(tagDriverEndpoint(output))throw std::runtime_error("Выберите физические наушники: вывод обратно в TAG создаёт петлю звука.");
    auto name=found->name;
    std::transform(name.begin(),name.end(),name.begin(),[](wchar_t c){return static_cast<wchar_t>(towlower(c));});
    for(auto forbidden:{L"cable",L"voicemeeter",L"broadcast"})
        if(name.find(forbidden)!=std::wstring::npos)throw std::runtime_error("Выберите физические наушники, не виртуальное устройство.");
    owner_=CreateMutexW(nullptr,FALSE,L"Local\\MicNoize.HeadphoneOwner");
    if(!owner_)throw std::runtime_error("Headphone ownership lock failed");
    if(GetLastError()==ERROR_ALREADY_EXISTS){CloseHandle(owner_);owner_=nullptr;throw std::runtime_error("Headphones already in use by another instance");}
    ResetEvent(stop_);ResetEvent(data_);ResetEvent(ready_);++epoch_;processed=0;drops=0;
    {std::lock_guard lock(mutex_);message_.clear();}state=1;
    try {dsp_=std::thread(&Headphones::dspLoop,this,denoise);io_=std::thread(&Headphones::ioLoop,this,output);}
    catch(...){stop();throw;}
}
void Headphones::dspLoop(bool denoise) {
    try {
        Config c;c.version=2;c.intensity=intensity;c.sdk=projectRoot()/L"vendor/nvidia-afx-3.0.0";
        std::unique_ptr<Afx> leftFx,rightFx;
        if(denoise){leftFx=std::make_unique<Afx>(c);if(WaitForSingleObject(stop_,0)==WAIT_OBJECT_0)return;rightFx=std::make_unique<Afx>(c);}
        PitchEffect leftPitch,rightPitch;
        GrainReverse leftReverse,rightReverse;bool reversing=false;
        std::array<StereoSample,block> samples{};
        std::array<float,block> left{},right{},outLeft{},outRight{};
        unsigned generation=epoch_;float strength=intensity;
        SetEvent(ready_);Mmcss priority;
        HANDLE waits[]={stop_,data_};
        while(WaitForMultipleObjects(2,waits,FALSE,INFINITE)==WAIT_OBJECT_0+1) {
            while(WaitForSingleObject(stop_,0)!=WAIT_OBJECT_0 && captured_.pop(samples.data(),block)) {
                const unsigned current=epoch_;
                if(samples.front().epoch!=current || samples.back().epoch!=current)continue;
                if(generation!=current){if(leftFx){leftFx->reset();rightFx->reset();}leftPitch.reset();rightPitch.reset();leftReverse.reset();rightReverse.reset();generation=current;}
                for(unsigned i=0;i<block;++i){left[i]=samples[i].left;right[i]=samples[i].right;}
                if(leftFx) {
                    const float next=intensity;
                    if(next!=strength){leftFx->strength(next);rightFx->strength(next);strength=next;}
                    leftFx->process(left.data(),outLeft.data());rightFx->process(right.data(),outRight.data());
                } else {outLeft=left;outRight=right;}
                const int tone=pitch;
                leftPitch.process(outLeft.data(),block,tone,tone!=0);
                rightPitch.process(outRight.data(),block,tone,tone!=0);
                // Off adds no latency; each switch starts from empty grains (no old tail).
                if(const bool want=reverse;want!=reversing){leftReverse.reset();rightReverse.reset();reversing=want;}
                if(reversing){leftReverse.process(outLeft.data(),block);rightReverse.process(outRight.data(),block);}
                for(unsigned i=0;i<block;++i){
                    if(!std::isfinite(outLeft[i])||!std::isfinite(outRight[i]))throw std::runtime_error("Non-finite headphone audio");
                    samples[i]={std::clamp(outLeft[i],-1.f,1.f),std::clamp(outRight[i],-1.f,1.f),current};
                }
                if(!cleaned_.push(samples.data(),block))++drops;
                ++processed;
            }
        }
    }catch(const std::exception& e){fail(e);}
}
void Headphones::ioLoop(std::wstring outputId) {
    try {
        Com com;HANDLE readyEvents[]={stop_,ready_};
        if(WaitForMultipleObjects(2,readyEvents,FALSE,INFINITE)!=WAIT_OBJECT_0+1)return;
        Event renderEvent;
        ensureTagHost();HeadphoneClient tag;
        Stream output;output.open(outputId,false,renderEvent.h,5);
        if(output.channels>2 || output.capacity>8192)throw std::runtime_error("Выберите моно или стерео наушники.");
        ComPtr<IAudioRenderClient> render;ComPtr<IAudioClockAdjustment> adjust;
        check(output.client->GetService(IID_PPV_ARGS(&render)),"Headphone render");
        check(output.client->GetService(IID_PPV_ARGS(&adjust)),"Headphone clock");
        check(adjust->SetSampleRate(rate),"Headphone rate");
        HANDLE timer=CreateWaitableTimerExW(nullptr,nullptr,CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,TIMER_ALL_ACCESS);
        if(!timer)throw std::runtime_error("Headphone timer failed");
        struct Timer {HANDLE h;~Timer(){CloseHandle(h);}} timerGuard{timer};
        std::array<float,8192> left{},right{};std::array<StereoSample,8192> frames{};
        bool active=false,primed=false;TagClock clock;Drift drift;Ramp gain;
        auto last=std::chrono::steady_clock::now(),adjusted=last;
        HANDLE events[]={stop_,renderEvent.h,timer};
        LARGE_INTEGER initialDue{};initialDue.QuadPart=-20000;
        if(!SetWaitableTimer(timer,&initialDue,2,nullptr,nullptr,FALSE))throw std::runtime_error("Headphone timer start failed");
        state=2;Mmcss priority;
        for(;;) {
            const auto wait=WaitForMultipleObjects(3,events,FALSE,INFINITE);
            if(wait==WAIT_OBJECT_0)break;
            if(wait==WAIT_FAILED)throw std::runtime_error("Headphone wait failed");
            tag.handleEvent();const auto now=std::chrono::steady_clock::now();
            const bool running=tag.running();
            if(running!=active){
                active=running;clock={};last=now;++epoch_;cleaned_.trim(0);primed=false;drift={};gain=Ramp{};
                if(active){
                    LARGE_INTEGER due{};due.QuadPart=-20000;
                    if(!SetWaitableTimer(timer,&due,2,nullptr,nullptr,FALSE))throw std::runtime_error("Headphone timer start failed");
                    output.start();
                }else{
                    check(output.client->Stop(),"Headphone stop");output.started=false;
                    check(output.client->Reset(),"Headphone reset");
                }
            }
            if(!active)continue;
            auto n=clock.take(std::chrono::duration<double>(now-last).count(),0,std::min(8192u,tag.capacity()/2));last=now;
            if(n){
                tag.read(left.data(),right.data(),n);
                const unsigned generation=epoch_;
                for(unsigned i=0;i<n;++i)frames[i]={left[i],right[i],generation};
                if(!captured_.push(frames.data(),n)){++drops;++epoch_;}
                SetEvent(data_);
            }
            UINT32 padding=0;check(output.client->GetCurrentPadding(&padding),"Headphone padding");
            if(padding>output.capacity)throw std::runtime_error("Headphone invalid padding");
            n=output.capacity-padding;
            if(cleaned_.size()>4800){cleaned_.trim(1920);++drops;primed=false;drift={};}
            if(!primed && cleaned_.size()>=1920+n)primed=true;
            if(n){
                const bool have=primed && cleaned_.pop(frames.data(),n);
                if(!have)primed=false;
                BYTE* bytes=nullptr;check(render->GetBuffer(n,&bytes),"Headphone buffer");
                const auto generation=epoch_.load();const bool silent=muted;
                const float level=volume;
                auto dst=reinterpret_cast<float*>(bytes);
                for(unsigned i=0;i<n;++i){
                    const float g=gain.next(level);
                    const bool valid=have && !silent && frames[i].epoch==generation;
                    const float l=valid?frames[i].left*g:0,r=valid?frames[i].right*g:0;
                    if(output.channels==1)dst[i]=(l+r)*0.5f;
                    else {dst[i*2]=l;dst[i*2+1]=r;}
                }
                check(render->ReleaseBuffer(n,0),"Headphone release");
            }
            if(primed && now-adjusted>=std::chrono::milliseconds(100)){
                check(adjust->SetSampleRate(static_cast<float>(rate*(1+drift.update(static_cast<double>(cleaned_.size())-1920)))),"Headphone drift");adjusted=now;
            }
        }
    }catch(const std::exception& e){fail(e);}
}
void checkHeadphones(const std::wstring& output,bool denoise) {
    Com com;Headphones h;h.muted=true;h.volume=0;h.start(output,denoise);
    for(unsigned i=0;i<600 && h.state==1;++i)Sleep(50);
    if(h.state!=2)throw std::runtime_error("Headphone start: "+utf8(h.message()));
    std::wstring endpoint;
    for(unsigned i=0;i<100 && endpoint.empty();++i){
            for(const auto& d:devices(false))if(tagDriverEndpoint(d.id,L"MicNoize Headphones.Render.Topology")) {
            if(!endpoint.empty())throw std::runtime_error("Ambiguous TAG headphone endpoint");endpoint=d.id;
        }
        if(endpoint.empty())Sleep(50);
    }
    if(endpoint.empty())throw std::runtime_error("TAG headphones did not appear");
    bool feedbackRejected=false;
    try{Headphones feedback;feedback.start(endpoint,false);}catch(const std::exception& error){feedbackRejected=std::string(error.what()).find("TAG")!=std::string::npos;}
    if(!feedbackRejected)throw std::runtime_error("TAG headphone feedback route was not rejected by driver identity");
    Event event;Stream source;source.open(endpoint,false,event.h,5);
    if(source.channels!=2)throw std::runtime_error("TAG headphones not stereo");
    ComPtr<IAudioRenderClient> render;check(source.client->GetService(IID_PPV_ARGS(&render)),"Test render");
    source.start();unsigned frames=0;
    const auto end=std::chrono::steady_clock::now()+std::chrono::seconds(3);
    while(std::chrono::steady_clock::now()<end){
        if(h.state==3)throw std::runtime_error(utf8(h.message()));
        WaitForSingleObject(event.h,100);UINT32 padding=0;
        check(source.client->GetCurrentPadding(&padding),"Test padding");
        const auto n=source.capacity-padding;
        if(n){BYTE* data=nullptr;check(render->GetBuffer(n,&data),"Test buffer");
            for(unsigned i=0;i<n;++i){auto p=reinterpret_cast<float*>(data)+2*i;p[0]=0.05f*std::sin((frames+i)*440.0f*6.2831853f/rate);p[1]=0.025f*std::sin((frames+i)*660.0f*6.2831853f/rate);}
            check(render->ReleaseBuffer(n,0),"Test release");frames+=n;
        }
    }
    const unsigned blocks=h.processed;const unsigned dropped=h.drops;
    h.stop();if(h.state!=0 || blocks<100)throw std::runtime_error("Headphone pipeline did not process enough blocks");
    const unsigned stopped=h.processed;Sleep(100);if(h.processed!=stopped)throw std::runtime_error("Headphone DSP still running after stop");
    std::cout<<"HEADPHONES CHECK PASSED: stereo synthetic input, physical output muted, TAG feedback rejected by identity; denoise="<<denoise<<", blocks="<<blocks<<", drops="<<dropped<<"; stopped\n";
}
}
