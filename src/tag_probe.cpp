// Explicit diagnostics for the signed TAG driver. No microphone capture, models,
// driver installation or automatic host termination. --shared-only explicitly
// sets only the live host-confirmed endpoint's exclusive-access policy.
#include "tag.hpp"
#include "tag_link.hpp"
#include <audioclient.h>
#include <devicetopology.h>
#include <functiondiscoverykeys_devpkey.h>
#include <endpointvolume.h>
#include <wrl/client.h>
#include <iostream>
#include <iomanip>

using namespace ThinAudioGateway;
using Microsoft::WRL::ComPtr;

static void checked(HRESULT hr,const char* action) {
    if(FAILED(hr)) {
        char message[200];sprintf_s(message,"%s: HRESULT 0x%08lX",action,static_cast<unsigned long>(hr));
        throw std::runtime_error(message);
    }
}
struct Com {
    Com(){checked(CoInitializeEx(nullptr,COINIT_MULTITHREADED),"COM");}
    ~Com(){CoUninitialize();}
};
static std::wstring property(IMMDevice* device,REFPROPERTYKEY key) {
    ComPtr<IPropertyStore> store;checked(device->OpenPropertyStore(STGM_READ,&store),"Property store");
    PROPVARIANT value{};checked(store->GetValue(key,&value),"Property");
    std::wstring result=value.vt==VT_LPWSTR && value.pwszVal?value.pwszVal:L"";
    PropVariantClear(&value);return result;
}
static void endpoints() {
    Com com;ComPtr<IMMDeviceEnumerator> enumerator;ComPtr<IMMDeviceCollection> list;
    checked(CoCreateInstance(__uuidof(MMDeviceEnumerator),nullptr,CLSCTX_ALL,IID_PPV_ARGS(&enumerator)),"Enumerator");
    checked(enumerator->EnumAudioEndpoints(eCapture,DEVICE_STATEMASK_ALL,&list),"Endpoints");
    UINT count=0;checked(list->GetCount(&count),"Endpoint count");
    for(UINT i=0;i<count;++i) {
        ComPtr<IMMDevice> device;checked(list->Item(i,&device),"Endpoint");
        const auto instance=property(device.Get(),PKEY_Device_InstanceId);
        ComPtr<IDeviceTopology> topology;ComPtr<IConnector> connector;
        std::wstring adapter;
        if(SUCCEEDED(device->Activate(__uuidof(IDeviceTopology),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(topology.GetAddressOf()))) &&
           SUCCEEDED(topology->GetConnector(0,&connector))) {
            LPWSTR text=nullptr;
            if(SUCCEEDED(connector->GetDeviceIdConnectedTo(&text)) && text) adapter=text;
            CoTaskMemFree(text);
        }
        // Diagnostic filtering only: runtime routing must bind the driver/line identity.
        const auto name=property(device.Get(),PKEY_Device_FriendlyName);
        if(name.find(L"Thin Audio Gateway")==std::wstring::npos && adapter.find(L"thinaudiogateway")==std::wstring::npos)continue;
        if(!adapter.empty()) {
            ComPtr<IMMDevice> adapterDevice;
            const auto hr=enumerator->GetDevice(adapter.c_str(),&adapterDevice);
            if(SUCCEEDED(hr))std::cout<<"adapter_name="<<mic::utf8(property(adapterDevice.Get(),PKEY_Device_FriendlyName))
                <<" adapter_instance="<<mic::utf8(property(adapterDevice.Get(),PKEY_Device_InstanceId))
                <<" adapter_interface_name="<<mic::utf8(property(adapterDevice.Get(),PKEY_DeviceInterface_FriendlyName))<<'\n';
        }
        LPWSTR id=nullptr;checked(device->GetId(&id),"Endpoint ID");
        const std::wstring endpointId=id;CoTaskMemFree(id);
        DWORD state=0;checked(device->GetState(&state),"Endpoint state");
        std::cout<<"endpoint state="<<state<<" id="<<mic::utf8(endpointId)<<" name="<<mic::utf8(name)
            <<" instance="<<mic::utf8(instance)<<" topology="<<mic::utf8(adapter)<<'\n';
        if(state==DEVICE_STATE_ACTIVE) {
            ComPtr<IAudioEndpointVolume> volume;
            checked(device->Activate(__uuidof(IAudioEndpointVolume),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(volume.GetAddressOf())),"Endpoint volume");
            float min=0,max=0,step=0,scalar=0;BOOL mute=FALSE;DWORD hardware=0;
            checked(volume->GetVolumeRange(&min,&max,&step),"Volume range");
            checked(volume->GetMasterVolumeLevelScalar(&scalar),"Volume scalar");
            checked(volume->GetMute(&mute),"Mute");
            checked(volume->QueryHardwareSupport(&hardware),"Hardware volume support");
            std::cout<<"volume min_db="<<min<<" max_db="<<max<<" step_db="<<step<<" scalar="<<scalar<<" mute="<<mute<<" hardware="<<hardware<<'\n';
        }
    }
}
struct Driver {
    HMODULE dll=nullptr;TagDriver* driver=nullptr;TagPipe* pipe=nullptr;
    Driver() {
        try {
            const auto path=mic::projectRoot()/L"vendor/tag-2.0.0.1903-demo/apidll/x64/tagapi.dll";
            dll=LoadLibraryExW(path.c_str(),nullptr,LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR|LOAD_LIBRARY_SEARCH_SYSTEM32);
            if(!dll)throw std::runtime_error("Load TAG API");
            using Create=HRESULT (__stdcall*)(void**,const GUID*);
            auto create=reinterpret_cast<Create>(GetProcAddress(dll,"Driver_Create"));
            if(!create)throw std::runtime_error("Driver_Create export");
            const GUID product={0x4d699d4a,0x65a5,0x40ec,{0x98,0x75,0x8e,0x6d,0x5f,0xc0,0x1e,0x0c}};
            checked(create(reinterpret_cast<void**>(&driver),&product),"Create driver API");
            checked(driver->Find(),"Find driver");
            checked(driver->Open(),"Open driver (stop Mic Noize and its host for this explicit probe)");
        }catch(...){close();throw;}
    }
    ~Driver(){close();}
    void close(){if(pipe)pipe->Delete();if(driver)driver->Delete();if(dll)FreeLibrary(dll);}
    std::vector<VirtualLineDesc> lines() {
        DriverInfo info{};checked(driver->GetInfo(info),"Driver info");
        std::cout<<"driver "<<info.VerMajor<<'.'<<info.VerMinor<<'.'<<info.VerRev<<'.'<<info.VerBuild
            <<" lines="<<info.NumLines<<" interface="<<mic::utf8(driver->GetInterface())<<'\n';
        if(info.NumLines>128)throw std::runtime_error("Invalid line count");
        std::vector<VirtualLineDesc> result(std::max(info.NumLines,1u));UINT count=0;
        checked(driver->GetLineList(result.data(),static_cast<LocSize>(result.size()*sizeof(VirtualLineDesc)),count),"Line list");
        if(count>result.size())throw std::runtime_error("Line list overflow");
        result.resize(count);
        for(const auto& line:result)std::cout<<"line id="<<line.Id<<" capture="<<line.Capture
            <<" ks="<<mic::utf8(line.KsName)<<" endpoint="<<mic::utf8(line.EpName)<<'\n';
        return result;
    }
};
static void printParams(const char* label,const VolumeControlParamsDesc& p) {
    std::cout<<label<<" enabled="<<p.Enabled<<" min_db="<<p.MinDb<<" max_db="<<p.MaxDb<<" step_db="<<p.StepDb<<'\n';
}
static int volumeRange(unsigned lineId) {
    Driver d;const auto lines=d.lines();
    if(std::none_of(lines.begin(),lines.end(),[&](const auto& line){return line.Id==lineId && line.Capture;}))
        throw std::runtime_error("Requested capture line is absent; no line was created");
    checked(d.driver->CreatePipe(d.pipe,lineId,true),"Open probe pipe");
    const auto original=d.pipe->GetCommonData().Drv.FI.VolumeControlParams;
    printParams("original",original);
    LineReq_SetVolumeControlParams request{};request.Params={true,-96,0,1};
    const HRESULT result=d.pipe->ControlRequest(IoCtl_Line_SetVolumeControlParams,request,sizeof(request));
    std::cout<<"SetVolumeControlParams HRESULT=0x"<<std::hex<<std::setw(8)<<std::setfill('0')<<static_cast<unsigned long>(result)<<std::dec<<'\n';
    const auto actual=d.pipe->GetCommonData().Drv.FI.VolumeControlParams;
    printParams("readback",actual);
    if(FAILED(result)) {
        // A no-op request distinguishes an unsupported command from this chosen range.
        request.Params=original;
        const auto noop=d.pipe->ControlRequest(IoCtl_Line_SetVolumeControlParams,request,sizeof(request));
        std::cout<<"Original-parameters request HRESULT=0x"<<std::hex<<std::setw(8)<<static_cast<unsigned long>(noop)<<std::dec<<'\n';
    }
    // Restore even on a failed call if the reported state changed.
    const bool changed=actual.Enabled!=original.Enabled || actual.MinDb!=original.MinDb || actual.MaxDb!=original.MaxDb || actual.StepDb!=original.StepDb;
    if(changed) {
        request.Params=original;
        checked(d.pipe->ControlRequest(IoCtl_Line_SetVolumeControlParams,request,sizeof(request)),"Restore volume parameters");
        const auto restored=d.pipe->GetCommonData().Drv.FI.VolumeControlParams;
        printParams("restored",restored);
        if(restored.Enabled!=original.Enabled || restored.MinDb!=original.MinDb || restored.MaxDb!=original.MaxDb || restored.StepDb!=original.StepDb)
            throw std::runtime_error("Volume parameters were not restored");
    }
    if(FAILED(result) || !actual.Enabled || actual.MaxDb!=0) {
        std::cout<<"BLOCKED: signed driver did not accept the zero-dB range; no gain compensation may be removed\n";return 2;
    }
    std::cout<<"API ACCEPTED: endpoint readback and measured shared/exclusive capture are still required\n";return 0;
}
struct Handle {
    HANDLE value=nullptr;
    ~Handle(){if(value)CloseHandle(value);}
};
static void signal(const std::wstring& endpointId,bool exclusive) {
    // Own the same producer lock as Engine. Never send test audio beside a live producer.
    Handle owner{CreateMutexW(nullptr,FALSE,L"Local\\MicNoize.TAG")};
    if(!owner.value)throw std::runtime_error("Producer mutex");
    if(GetLastError()==ERROR_ALREADY_EXISTS)throw std::runtime_error("Stop processing before the signal probe");
    Com com;ComPtr<IMMDeviceEnumerator> enumerator;ComPtr<IMMDevice> endpoint;
    checked(CoCreateInstance(__uuidof(MMDeviceEnumerator),nullptr,CLSCTX_ALL,IID_PPV_ARGS(&enumerator)),"Enumerator");
    checked(enumerator->GetDevice(endpointId.c_str(),&endpoint),"Signal endpoint");
    ComPtr<IMMEndpoint> endpointKind;checked(endpoint.As(&endpointKind),"Signal endpoint kind");
    EDataFlow flow=eAll;checked(endpointKind->GetDataFlow(&flow),"Signal endpoint direction");
    if(flow!=eCapture)throw std::runtime_error("Signal probe requires a capture endpoint");
    // Require a TAG adapter, not just a user-editable display name.
    ComPtr<IDeviceTopology> topology;ComPtr<IConnector> connector;
    checked(endpoint->Activate(__uuidof(IDeviceTopology),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(topology.GetAddressOf())),"Signal topology");
    checked(topology->GetConnector(0,&connector),"Signal connector");
    LPWSTR adapterText=nullptr;checked(connector->GetDeviceIdConnectedTo(&adapterText),"Signal adapter");
    const std::wstring adapter=adapterText?adapterText:L"";CoTaskMemFree(adapterText);
    if(adapter.find(L"root#thinaudiogateway_4d699d4a#")==std::wstring::npos)
        throw std::runtime_error("Signal probe requires the TAG capture endpoint");
    ComPtr<IAudioEndpointVolume> volume;
    checked(endpoint->Activate(__uuidof(IAudioEndpointVolume),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(volume.GetAddressOf())),"Signal volume");
    float scalar=0;BOOL muted=FALSE;
    checked(volume->GetMasterVolumeLevelScalar(&scalar),"Signal volume scalar");
    checked(volume->GetMute(&muted),"Signal mute");
    if(scalar<0.9999f || muted)throw std::runtime_error("Set only the virtual TAG endpoint to 100% and unmute before measuring");
    ComPtr<IAudioClient> client;ComPtr<IAudioCaptureClient> capture;
    checked(endpoint->Activate(__uuidof(IAudioClient),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(client.GetAddressOf())),"Signal client");
    WAVEFORMATEXTENSIBLE format{};
    format.Format={WAVE_FORMAT_EXTENSIBLE,2,48000,384000,8,32,22};
    format.Samples.wValidBitsPerSample=32;format.dwChannelMask=SPEAKER_FRONT_LEFT|SPEAKER_FRONT_RIGHT;
    format.SubFormat=exclusive?KSDATAFORMAT_SUBTYPE_PCM:KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    const auto mode=exclusive?AUDCLNT_SHAREMODE_EXCLUSIVE:AUDCLNT_SHAREMODE_SHARED;
    const DWORD flags=exclusive?0:AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM|AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    checked(client->Initialize(mode,flags,1000000,exclusive?1000000:0,&format.Format,nullptr),"Initialize signal capture");
    checked(client->GetService(IID_PPV_ARGS(&capture)),"Capture service");
    constexpr float amplitude=0.1f/31.6227766017f; // Never send physical microphone audio.
    std::atomic<bool> producerReady=false;std::string producerError;
    std::jthread producer([&](std::stop_token stop){
        try {
            mic::TagClient tag;mic::TagClock clock;std::array<float,16384> samples{};
            auto last=std::chrono::steady_clock::now();uint64_t position=0;producerReady=true;
            while(!stop.stop_requested()) {
                Sleep(2);tag.handleEvent();const auto now=std::chrono::steady_clock::now();
                if(tag.running()) {
                    const auto count=clock.take(std::chrono::duration<double>(now-last).count(),0,std::min(tag.capacity()/2,16384u));
                    for(unsigned i=0;i<count;++i)samples[i]=amplitude*static_cast<float>(std::sin(2*3.141592653589793*1000*(position+i)/48000));
                    if(count)tag.write(samples.data(),count);
                    position+=count;
                }else clock={};
                last=now;
            }
        }catch(const std::exception& e){producerError=e.what();}
    });
    for(unsigned i=0;i<100 && !producerReady;++i)Sleep(10);
    if(!producerReady){producer.request_stop();producer.join();throw std::runtime_error("Signal producer failed: "+producerError);}
    bool started=false;uint64_t frames=0;double square=0,peak=0;unsigned discontinuities=0;
    try {
        checked(client->Start(),"Start signal capture");started=true;
        const auto begin=std::chrono::steady_clock::now();
        while(std::chrono::steady_clock::now()-begin<std::chrono::seconds(3)) {
            Sleep(2);UINT32 count=0;checked(capture->GetNextPacketSize(&count),"Signal packet");
            while(count) {
                BYTE* data=nullptr;DWORD packetFlags=0;
                checked(capture->GetBuffer(&data,&count,&packetFlags,nullptr,nullptr),"Signal buffer");
                if(std::chrono::steady_clock::now()-begin>std::chrono::milliseconds(500)) {
                    if(packetFlags&AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY)++discontinuities;
                    for(UINT32 i=0;i<count*2;++i) {
                        const double v=packetFlags&AUDCLNT_BUFFERFLAGS_SILENT?0:
                            (exclusive?reinterpret_cast<const int32_t*>(data)[i]/2147483648.0:reinterpret_cast<const float*>(data)[i]);
                        square+=v*v;peak=std::max(peak,std::abs(v));
                    }
                    frames+=count;
                }
                checked(capture->ReleaseBuffer(count),"Release signal packet");
                checked(capture->GetNextPacketSize(&count),"Next signal packet");
            }
        }
        checked(client->Stop(),"Stop signal capture");started=false;
    }catch(...){if(started)client->Stop();producer.request_stop();producer.join();throw;}
    producer.request_stop();producer.join();
    if(!producerError.empty())throw std::runtime_error(producerError);
    if(frames<48000 || !std::isfinite(square) || peak==0)throw std::runtime_error("Insufficient finite signal received");
    const double rms=std::sqrt(square/(frames*2));
    std::cout<<"MEASURED mode="<<(exclusive?"exclusive":"shared")<<" frames="<<frames<<" input_peak="<<amplitude
        <<" output_peak="<<peak<<" rms="<<rms<<" peak_gain="<<peak/amplitude<<" discontinuities="<<discontinuities
        <<"; synthetic signal only, no audio file\n";
}
static void sharedOnly(bool apply,bool challenge=false,bool checkWrite=false) {
    Com com;mic::TagEndpointStatus status;
    if(!mic::readTagEndpointStatus(status) || !status.ready)throw std::runtime_error("Confirmed live TAG endpoint required");
    ComPtr<IMMDeviceEnumerator> enumerator;ComPtr<IMMDevice> endpoint;
    checked(CoCreateInstance(__uuidof(MMDeviceEnumerator),nullptr,CLSCTX_ALL,IID_PPV_ARGS(&enumerator)),"Enumerator");
    checked(enumerator->GetDevice(status.endpoint,&endpoint),"Confirmed TAG endpoint");
    if(checkWrite) {
        HANDLE token=nullptr;TOKEN_ELEVATION elevation{};DWORD size=0;
        if(!OpenProcessToken(GetCurrentProcess(),TOKEN_QUERY,&token))throw std::runtime_error("Policy check token unavailable");
        const bool known=GetTokenInformation(token,TokenElevation,&elevation,sizeof(elevation),&size)!=0;CloseHandle(token);
        if(!known || elevation.TokenIsElevated)throw std::runtime_error("Run policy write check without administrator elevation");
        if(!mic::tagDriverEndpoint(status.endpoint))throw std::runtime_error("Policy check endpoint is not TAG");
        constexpr PROPERTYKEY key={{0xb3f8fa53,0x0004,0x438e,{0x90,0x03,0x51,0xa4,0x6e,0x13,0x9b,0xfc}},3};
        ComPtr<IPropertyStore> store;checked(endpoint->OpenPropertyStore(STGM_READ,&store),"Policy check read");
        PROPVARIANT value{};checked(store->GetValue(key,&value),"Policy check value");
        const bool disabled=value.vt==VT_UI4 && value.ulVal==0;PropVariantClear(&value);store.Reset();
        if(!disabled)throw std::runtime_error("Policy write check requires an already shared-only endpoint");
        value.vt=VT_UI4;value.ulVal=0;mic::setEndpointProperty(endpoint.Get(),key,value);
        checked(endpoint->OpenPropertyStore(STGM_READ,&store),"Policy check readback");
        checked(store->GetValue(key,&value),"Policy check written value");
        const bool verified=value.vt==VT_UI4 && value.ulVal==0;PropVariantClear(&value);
        if(!verified)throw std::runtime_error("Policy write check readback failed");
        std::cout<<"PASS: unelevated PolicyConfig write and readback; shared-only policy retained\n";
    }
    if(apply){std::cout<<"shared_only_changed="<<mic::holdTagEndpointSharedMode(endpoint.Get())<<'\n';return;}
    if(challenge) {
        if(status.hostBuild!=mic::tagHostBuild)throw std::runtime_error("Matching new host required for policy guard test");
        constexpr PROPERTYKEY key={{0xb3f8fa53,0x0004,0x438e,{0x90,0x03,0x51,0xa4,0x6e,0x13,0x9b,0xfc}},3};
        try {
            ComPtr<IPropertyStore> store;
            PROPVARIANT value{};value.vt=VT_UI4;value.ulVal=1;
            mic::setEndpointProperty(endpoint.Get(),key,value);
            const auto started=GetTickCount64();bool restored=false;
            do {
                Sleep(10);checked(endpoint->OpenPropertyStore(STGM_READ,&store),"Policy challenge read");
                PROPVARIANT current{};checked(store->GetValue(key,&current),"Policy challenge value");
                restored=current.vt==VT_UI4 && current.ulVal==0;PropVariantClear(&current);store.Reset();
            }while(!restored && GetTickCount64()-started<1500);
            if(!restored)throw std::runtime_error("Host did not restore shared-only policy within 1500 ms");
            std::cout<<"policy_restored_ms="<<GetTickCount64()-started<<'\n';
        }catch(...){mic::holdTagEndpointSharedMode(endpoint.Get());throw;}
    }
    WAVEFORMATEXTENSIBLE format{};format.Format={WAVE_FORMAT_EXTENSIBLE,2,48000,384000,8,32,22};
    format.Samples.wValidBitsPerSample=32;format.dwChannelMask=SPEAKER_FRONT_LEFT|SPEAKER_FRONT_RIGHT;format.SubFormat=KSDATAFORMAT_SUBTYPE_PCM;
    ComPtr<IAudioClient> exclusive;
    checked(endpoint->Activate(__uuidof(IAudioClient),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(exclusive.GetAddressOf())),"Exclusive policy test client");
    const auto hr=exclusive->Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE,0,1000000,1000000,&format.Format,nullptr);
    std::cout<<"exclusive_initialize=0x"<<std::hex<<static_cast<unsigned long>(hr)<<std::dec<<'\n';
    if(hr!=AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED)throw std::runtime_error("Exclusive access was not explicitly denied by policy");
    exclusive.Reset();format.SubFormat=KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    ComPtr<IAudioClient> shared[2];ComPtr<IAudioCaptureClient> capture[2];
    for(unsigned i=0;i<2;++i) {
        checked(endpoint->Activate(__uuidof(IAudioClient),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(shared[i].GetAddressOf())),"Shared policy test client");
        checked(shared[i]->Initialize(AUDCLNT_SHAREMODE_SHARED,AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM|AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,1000000,0,&format.Format,nullptr),"Shared policy initialize");
        checked(shared[i]->GetService(IID_PPV_ARGS(&capture[i])),"Shared policy capture service");
        checked(shared[i]->Start(),"Shared policy capture start");
    }
    Sleep(100);
    for(unsigned i=0;i<2;++i) {
        UINT32 frames=0;checked(capture[i]->GetNextPacketSize(&frames),"Shared policy packet");
        checked(shared[i]->Stop(),"Shared policy capture stop");
        if(!frames)throw std::runtime_error("Shared client received no frames");
        std::cout<<"shared_client="<<i+1<<" packet_frames="<<frames<<'\n';
    }
    std::cout<<"PASS: exclusive denied; two simultaneous shared capture clients receive frames; no audio saved\n";
}
int wmain(int argc,wchar_t** argv) {
    std::cout<<std::unitbuf;
    try {
        if(argc==2 && std::wstring_view(argv[1])==L"--shared-only"){sharedOnly(true);return 0;}
        if(argc==2 && std::wstring_view(argv[1])==L"--check-shared-only"){sharedOnly(false);return 0;}
        if(argc==2 && std::wstring_view(argv[1])==L"--check-shared-policy-write"){sharedOnly(false,false,true);return 0;}
        if(argc==2 && std::wstring_view(argv[1])==L"--check-shared-guard"){sharedOnly(false,true);return 0;}
        if(argc==2 && std::wstring_view(argv[1])==L"--endpoints"){endpoints();return 0;}
        if(argc==3 && std::wstring_view(argv[1])==L"--host-file") {
            const bool compatible=mic::tagHostFileCompatible(argv[2]);
            std::cout<<"compatible="<<compatible<<" required_protocol="<<mic::tagProtocolVersion<<" required_build="<<mic::tagHostBuild<<'\n';
            return compatible?0:2;
        }
        if(argc==2 && std::wstring_view(argv[1])==L"--task-warning") {std::cout<<mic::tagTaskWarning()<<'\n';return 0;}
        if(argc==2 && std::wstring_view(argv[1])==L"--device-state") {std::string detail;const auto state=mic::tagDeviceState(detail);std::cout<<"state="<<static_cast<unsigned>(state)<<" detail="<<detail<<'\n';return 0;}
        if((argc==2 && std::wstring_view(argv[1])==L"--no-listeners") || (argc==3 && std::wstring_view(argv[1])==L"--idle-watch")) {
            const auto seconds=argc==3?std::stoi(argv[2]):0;
            if(seconds<0 || seconds>120)throw std::runtime_error("Idle watch must be at most 120 seconds");
            mic::TagEndpointStatus status;
            if(!mic::readTagEndpointStatus(status) || !status.ready)throw std::runtime_error("Host not ready for idle measurement");
            const auto mapping=OpenFileMappingW(FILE_MAP_READ,FALSE,L"Local\\MicNoize.TagLink.v2");
            if(!mapping)throw std::runtime_error("Audio status mapping unavailable");
            struct Close {HANDLE h;~Close(){CloseHandle(h);}} close{mapping};
            const auto packet=static_cast<const mic::TagPacketV2*>(MapViewOfFile(mapping,FILE_MAP_READ,0,0,sizeof(mic::TagPacketV2)));
            if(!packet)throw std::runtime_error("Audio status view unavailable");
            struct Unmap {const void* p;~Unmap(){UnmapViewOfFile(p);}} unmap{packet};
            const auto end=GetTickCount64()+seconds*1000;
            do {
                // Aligned DWORD load only: no producer, audio command, audio mutex or capture stream.
                const auto active=std::atomic_ref<const unsigned>(packet->running).load(std::memory_order_acquire);
                if(!mic::tagHeaderValid(*packet,status.hostId) || active>1 || !mic::tagEndpointHostAlive(status))throw std::runtime_error("Audio status generation changed");
                if(active){std::cout<<"capture_active=1; idle acceptance needs all capture clients closed\n";return 2;}
                if(seconds)Sleep(100);
            }while(GetTickCount64()<end);
            std::cout<<"capture_active=0 observed_seconds="<<seconds<<'\n';return 0;
        }
        if(argc==2 && std::wstring_view(argv[1])==L"--host-status") {
            mic::TagEndpointStatus status;
            if(!mic::readTagEndpointStatus(status))throw std::runtime_error("No endpoint status publisher");
            std::cout<<"host_alive="<<mic::tagEndpointHostAlive(status)<<" ready="<<status.ready<<" line="<<status.line
                <<" protocol="<<status.audioVersion<<" build="<<status.hostBuild<<" compensation="<<status.compensation<<" endpoint="<<mic::utf8(status.endpoint)<<" error="<<status.error<<'\n';
            return status.ready?0:2;
        }
        if(argc==2 && std::wstring_view(argv[1])==L"--lines"){Driver d;d.lines();return 0;}
        if(argc==3 && (std::wstring_view(argv[1])==L"--signal-shared" || std::wstring_view(argv[1])==L"--signal-exclusive")) {
            signal(argv[2],std::wstring_view(argv[1])==L"--signal-exclusive");return 0;
        }
        if(argc==3 && std::wstring_view(argv[1])==L"--volume-range") {
            size_t used=0;const std::wstring value=argv[2];const auto id=std::stoul(value,&used);
            if(used!=value.size() || !id || value.front()==L'-')throw std::runtime_error("Expected a positive TAG line ID");
            return volumeRange(id);
        }
        std::cout<<"mic_tag_probe --endpoints | --host-status | --host-file PATH | --lines | --volume-range LINE_ID | --signal-shared ENDPOINT_ID | --signal-exclusive ENDPOINT_ID | --shared-only | --check-shared-only | --check-shared-guard\n"
            <<"Signal probes require the host running and processing stopped. Line/range probes require the host stopped.\n";return 1;
    }catch(const std::exception& e){std::cerr<<e.what()<<'\n';return 1;}
}
