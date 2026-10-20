#pragma once
#include "audio.hpp"
#include "tag_protocol.hpp"
#include <winternl.h>
#include <mmreg.h>
#include <ks.h>
#include <ksmedia.h>
#include <winioctl.h>
#include <cmath>
#include <stdexcept>
#include <span>
#define _Debug 0
#define _BuildMode 2
#include <driver/DriverApi.h>
#include <apilib/userapi.h>
#undef _BuildMode
#undef _Debug

namespace mic {
inline bool tagLegacyRestartSafe(std::span<const ThinAudioGateway::VirtualLineDesc> lines) {
    // Only for the hash-pinned published v1 core: it preserves TAG defaults,
    // but deletes MicNoiseReducer-prefixed lines and can fall back to another input.
    unsigned microphones=0,headphones=0;
    for(const auto& line:lines) {
        if(!line.Id || !wmemchr(line.KsName,L'\0',std::size(line.KsName)))return false;
        if(line.Capture && !wcscmp(line.KsName,L"MicNoize Microphone"))++microphones;
        else if(!line.Capture && !wcscmp(line.KsName,L"MicNoize Headphones"))++headphones;
        else if(std::wstring_view(line.KsName).starts_with(L"MicNoiseReducer"))return false;
    }
    return microphones==1 && headphones<=1;
}
inline unsigned tagLineIdentity(std::span<const ThinAudioGateway::VirtualLineDesc> lines,bool capture,unsigned saved,unsigned reserved=0) {
    const auto* wanted=capture?L"MicNoize Microphone":L"MicNoize Headphones";
    unsigned found=0;
    for(const auto& line:lines) {
        if(!line.Id || !wmemchr(line.KsName,L'\0',std::size(line.KsName)))throw std::runtime_error("Invalid TAG line identity");
        if(line.Capture==capture && !wcscmp(line.KsName,wanted)) {
            if(found)throw std::runtime_error("Multiple TAG lines match Mic Noize; device repair required");
            found=line.Id;
        }
    }
    const auto occupied=[&](unsigned id){return id==reserved || std::any_of(lines.begin(),lines.end(),[&](const auto& line){return line.Id==id;});};
    if(found) {
        if((saved && saved!=found) || found==reserved)throw std::runtime_error("Saved TAG line identity conflicts with driver; device repair required");
        return found;
    }
    if(saved) {
        if(occupied(saved))throw std::runtime_error("Saved TAG line belongs to another device; device repair required");
        return saved;
    }
    // The demo driver recreates its own IDs 1/2 on boot. Never allocate those IDs.
    unsigned id=3;while(occupied(id))++id;return id;
}
inline bool tagDefaultMicrophone(const ThinAudioGateway::VirtualLineDesc& line) {
    return line.Id==1 && line.Capture && line.Type==ThinAudioGateway::VLT_Microphone &&
        wmemchr(line.KsName,L'\0',std::size(line.KsName)) && !wcscmp(line.KsName,L"TAG Microphone");
}
struct TagLineRepair {
    unsigned microphone=0,headphones=0;
    std::vector<unsigned> remove;
};
inline TagLineRepair tagLineRepair(std::span<const ThinAudioGateway::VirtualLineDesc> lines,unsigned microphone,unsigned headphones) {
    // Validate ambiguity before planning any destructive operation. Display names are irrelevant.
    tagLineIdentity(lines,true,0);tagLineIdentity(lines,false,0);
    TagLineRepair plan;std::vector<ThinAudioGateway::VirtualLineDesc> kept;
    for(const auto& line:lines) {
        const bool legacy=line.Id<=2 && ((line.Capture && !wcscmp(line.KsName,L"MicNoize Microphone")) ||
            (!line.Capture && !wcscmp(line.KsName,L"MicNoize Headphones")));
        if(tagDefaultMicrophone(line) || legacy)plan.remove.push_back(line.Id);else kept.push_back(line);
    }
    plan.microphone=tagLineIdentity(kept,true,microphone>=3?microphone:0,headphones>=3?headphones:0);
    plan.headphones=tagLineIdentity(kept,false,headphones>=3?headphones:0,plan.microphone);
    return plan;
}
// Only the documented user-mode TAG interface is used; the signed driver is unchanged.
class TagOutput {
    HMODULE module_=nullptr;
    ThinAudioGateway::TagDriver* driver_=nullptr;
    ThinAudioGateway::TagPipe* pipe_=nullptr;
    bool ownsDriver_=true;
    unsigned lineId_=0;
    unsigned headphoneId_=0;
    std::string headphoneIdentityError_;
    std::wstring ksName_;
    void release() {
        if(pipe_) pipe_->Delete();
        if(driver_ && ownsDriver_) driver_->Delete();
        if(module_) FreeLibrary(module_);
    }
    static void ok(HRESULT hr,const char* where) {
        if(FAILED(hr)) {
            char s[180]; sprintf_s(s,"%s: HRESULT 0x%08lX",where,static_cast<unsigned long>(hr));
            throw std::runtime_error(s);
        }
    }
public:
    explicit TagOutput(const std::filesystem::path& root,HANDLE event,bool capture=true,TagOutput* shared=nullptr,bool repairLines=false) {
        using namespace ThinAudioGateway;
        try {
            if(shared){
                if(!shared->headphoneIdentityError_.empty())throw std::runtime_error(shared->headphoneIdentityError_);
                driver_=shared->driver_;ownsDriver_=false;
            }else{
            module_=LoadLibraryExW((root/L"apidll/x64/tagapi.dll").c_str(),nullptr,LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR|LOAD_LIBRARY_SEARCH_SYSTEM32);
            if(!module_) ok(HRESULT_FROM_WIN32(GetLastError()),"Cannot load TAG API DLL");
            using Create=HRESULT (__stdcall*)(void**,const GUID*);
            auto create=reinterpret_cast<Create>(GetProcAddress(module_,"Driver_Create"));
            if(!create) throw std::runtime_error("TAG Driver_Create export missing");
            const GUID product={0x4d699d4a,0x65a5,0x40ec,{0x98,0x75,0x8e,0x6d,0x5f,0xc0,0x1e,0x0c}};
            ok(create(reinterpret_cast<void**>(&driver_),&product),"TAG create interface");
            ok(driver_->Find(),"Виртуальный аудиодрайвер не установлен");
            ok(driver_->Open(),"TAG open driver");
            }
            DriverInfo info{}; ok(driver_->GetInfo(info),"TAG driver info");
            if(info.VerMajor!=2 || info.NumLines>128) throw std::runtime_error("Unsupported TAG driver version/line count");
            std::vector<VirtualLineDesc> lines(std::max(1u,info.NumLines)); unsigned count=0;
            ok(driver_->GetLineList(lines.data(),static_cast<unsigned>(lines.size()*sizeof(VirtualLineDesc)),count),"TAG line list");
            if(count>lines.size()) throw std::runtime_error("Invalid TAG line count");
            lines.resize(count);
            // Preserve other lines, including driver defaults. A full driver is an
            // actionable creation failure, never permission to delete another endpoint.
            unsigned id=0;
            if(shared)id=tagLineIdentity(lines,capture,shared->headphoneId_,shared->lineId_);
            else {
                // Read and persist both reservations before the audio loop starts.
                // Windows DWORD values are atomic; an incomplete first save is safely retried.
                HKEY key=nullptr;
                ok(HRESULT_FROM_WIN32(RegCreateKeyExW(HKEY_CURRENT_USER,L"Software\\MicNoize\\TAG\\4d699d4a-65a5-40ec-9875-8e6d5fc01e0c",0,nullptr,0,KEY_QUERY_VALUE|KEY_SET_VALUE,nullptr,&key,nullptr)),"TAG saved identity access");
                struct Close {HKEY key;~Close(){RegCloseKey(key);}} close{key};
                auto read=[&](const wchar_t* name,bool allowZero=false) {
                    DWORD value=0,size=sizeof(value);const auto error=RegGetValueW(key,nullptr,name,RRF_RT_REG_DWORD,nullptr,&value,&size);
                    if(error==ERROR_FILE_NOT_FOUND)return 0u;
                    ok(HRESULT_FROM_WIN32(error),"TAG saved identity read");
                    if(!value && !allowZero)throw std::runtime_error("Invalid saved TAG line ID; device repair required");return static_cast<unsigned>(value);
                };
                auto microphone=read(L"MicrophoneLineId");unsigned headphones=0;
                try{headphones=read(L"HeadphoneLineId");}catch(const std::exception& error){if(repairLines)throw;headphoneIdentityError_=error.what();}
                const auto consent=read(L"ReplaceDefaultMicrophone",true);
                if(consent>1)throw std::runtime_error("Invalid TAG line repair consent");
                auto remove=[&](unsigned line){ok(driver_->DeleteLine(line),"TAG explicit line repair");std::erase_if(lines,[&](const auto& value){return value.Id==line;});};
                if(repairLines) {
                    const auto plan=tagLineRepair(lines,microphone,headphones);
                    const DWORD enabled=1;
                    ok(HRESULT_FROM_WIN32(RegSetValueExW(key,L"ReplaceDefaultMicrophone",0,REG_DWORD,reinterpret_cast<const BYTE*>(&enabled),sizeof(enabled))),"Save explicit TAG repair consent");
                    for(const auto line:plan.remove)remove(line);
                    microphone=plan.microphone;headphones=plan.headphones;
                } else if(consent) {
                    // Enabled only by the explicit repair checkbox. The signed demo
                    // recreates its unused default input each boot; it occupies the
                    // slot needed by Mic Noize Headphones. Never remove another line.
                    const auto found=std::find_if(lines.begin(),lines.end(),tagDefaultMicrophone);
                    if(found!=lines.end())remove(found->Id);
                }
                id=tagLineIdentity(lines,true,microphone,headphones);
                if(id<3)throw std::runtime_error("Старая линия Mic Noize: в «Восстановить устройство» включите перенос линий");
                if(headphoneIdentityError_.empty())try {
                    headphoneId_=tagLineIdentity(lines,false,headphones,id);
                    if(headphoneId_<3)throw std::runtime_error("Старая линия наушников: в «Восстановить устройство» включите перенос линий");
                }catch(const std::exception& error){if(repairLines)throw;headphoneIdentityError_=error.what();}
                auto save=[&](const wchar_t* name,unsigned value,unsigned previous) {
                    if(value!=previous)ok(HRESULT_FROM_WIN32(RegSetValueExW(key,name,0,REG_DWORD,reinterpret_cast<const BYTE*>(&value),sizeof(value))),"TAG saved identity write");
                };
                save(L"MicrophoneLineId",id,repairLines?0:microphone);
                if(headphoneIdentityError_.empty())save(L"HeadphoneLineId",headphoneId_,repairLines?0:headphones);
            }
            count=static_cast<unsigned>(lines.size());const std::span<const VirtualLineDesc> current(lines);
            if(std::none_of(current.begin(),current.end(),[&](const auto& line){return line.Id==id;})) {
                VirtualLineDesc line{}; line.Id=id; line.Capture=capture; line.Type=capture?VLT_Microphone:VLT_Headphones;
                wcscpy_s(line.KsName,capture?L"MicNoize Microphone":L"MicNoize Headphones");
                wcscpy_s(line.EpName,capture?L"Mic Noize":L"Mic Noize Headphones");
                const auto hr=driver_->CreateLine(line);
                if(FAILED(hr)) {
                    // Name the lines the driver already has: a refusal is otherwise just a code.
                    char s[160]; sprintf_s(s,"TAG create line %u: HRESULT 0x%08lX; driver has %u line(s):",id,static_cast<unsigned long>(hr),count);
                    std::string m=s;
                    for(unsigned i=0;i<count;++i){
                        m+=" [id "+std::to_string(lines[i].Id)+(lines[i].Capture?" capture":" render")+
                            " type "+std::to_string(static_cast<int>(lines[i].Type))+" \""+utf8(lines[i].KsName)+"\"]";
                    }
                    throw std::runtime_error(m);
                }
            }
            if(repairLines && std::none_of(current.begin(),current.end(),[&](const auto& line){return line.Id==headphoneId_;})) {
                VirtualLineDesc line{};line.Id=headphoneId_;line.Capture=false;line.Type=VLT_Headphones;
                wcscpy_s(line.KsName,L"MicNoize Headphones");wcscpy_s(line.EpName,L"Mic Noize Headphones");
                ok(driver_->CreateLine(line),"TAG headphone slot unavailable; unrelated lines were preserved");
            }
            ok(driver_->CreatePipe(pipe_,id,capture),"TAG open audio pipe");
            lineId_=id;
            ksName_=capture?L"MicNoize Microphone":L"MicNoize Headphones";
            for(unsigned i=0;i<count;++i)if(lines[i].Id==id)ksName_=lines[i].KsName;
            // 32-bit integer PCM; conversion from NVIDIA float happens only at the output.
            FormatRangeDesc range{48000,48000,32,32,2,2,4,4};
            WaveFormatDesc format{48000,32,2,4,SPEAKER_FRONT_LEFT|SPEAKER_FRONT_RIGHT};
            ok(pipe_->SetFormatRange(range),"TAG set format range");
            ok(pipe_->SetDefaultFormat(format),"TAG set default format");
            ok(pipe_->SetNotificationEvent(event),"TAG connect microphone");
        } catch(...) { release(); throw; }
    }
    ~TagOutput(){release();}
    TagOutput(const TagOutput&)=delete;
    TagOutput& operator=(const TagOutput&)=delete;
    unsigned lineId() const{return lineId_;}
    const std::wstring& ksName() const{return ksName_;}
    std::wstring driverInterface() const{return driver_->GetInterface();}
    bool handleEvent() {
        auto& c=pipe_->GetCommonData();
        if(!(c.Drv.FI.Flags&ThinAudioGateway::FilterFlags::WaitingForHost)) return false;
        if(c.Drv.FI.HasStream && c.Drv.FI.NotificationCode==ThinAudioGateway::HN_StreamCreation) {
            const auto& f=c.Drv.SI.Fmt;
            if(f.Format.nSamplesPerSec!=48000 || f.Format.nChannels!=2 || f.Format.wBitsPerSample!=32 ||
               f.Format.nBlockAlign!=8 || (f.Format.wFormatTag!=WAVE_FORMAT_PCM &&
               (f.Format.wFormatTag!=WAVE_FORMAT_EXTENSIBLE || !IsEqualGUID(f.SubFormat,KSDATAFORMAT_SUBTYPE_PCM)))) {
                pipe_->Notify(static_cast<NTSTATUS>(0xc000000d));
                return false; // Reject this client's format; keep the permanent host alive.
            }
        }
        ok(pipe_->Notify(0),"TAG acknowledge stream state");
        return true;
    }
    bool running() const {
        const auto& c=pipe_->GetCommonData();
        return c.Drv.FI.HasStream && c.Drv.SI.State==KSSTATE_RUN && c.Drv.SI.BufferFrames && c.Host.Buffer;
    }
    unsigned capacity() const {return pipe_->GetCommonData().Drv.SI.BufferFrames;}
    unsigned driverGaps() const {
        const auto& c=pipe_->GetCommonData(); return c.Drv.FI.DataOverflows+c.Drv.FI.DataUnderflows;
    }
    // The capture stream as the driver sees it: how much the host has written that Windows has not
    // read yet, and both positions. Diagnostics for TagBufferV1.
    TagBufferV1 bufferState() const {
        const auto& c=pipe_->GetCommonData();TagBufferV1 s;
        s.running=running();s.bufferFrames=c.Drv.SI.BufferFrames;s.framesBuffered=c.Drv.SI.FramesBuffered;
        s.clientPos=c.Drv.SI.ClientPos;s.devicePos=c.Host.DevicePos;s.overflows=c.Drv.SI.DataOverflows;
        s.underflows=c.Drv.SI.DataUnderflows;s.transferred=c.Drv.SI.FramesTransferred;
        return s;
    }
    void read(float* left,float* right,unsigned n) {
        auto& c=pipe_->GetCommonData();
        const auto frames=c.Drv.SI.BufferFrames,pos=c.Host.DevicePos;
        if(pipe_->IsCapt() || !running() || pos>=frames || n>frames/2) throw std::runtime_error("Invalid TAG render read");
        MemoryBarrier();
        const auto in=static_cast<const int32_t*>(c.Host.Buffer);
        for(unsigned i=0;i<n;++i) {
            const auto p=((pos+i)%frames)*2;
            left[i]=static_cast<float>(in[p]/2147483648.0);
            right[i]=static_cast<float>(in[p+1]/2147483648.0);
        }
        ok(pipe_->AdvanceDevicePosition(n),"TAG advance render position");
    }
    void write(const float* samples,unsigned n) {
        auto& c=pipe_->GetCommonData();
        const auto frames=c.Drv.SI.BufferFrames, pos=c.Host.DevicePos;
        if(!running() || pos>=frames || n>frames/2) throw std::runtime_error("Invalid TAG buffer write");
        auto out=static_cast<int32_t*>(c.Host.Buffer);
        for(unsigned i=0;i<n;++i) {
            const auto v=static_cast<int32_t>(std::clamp(static_cast<double>(samples[i]),-1.0,1.0)*2147483647.0);
            const auto p=((pos+i)%frames)*2; out[p]=v; out[p+1]=v;
        }
        MemoryBarrier();
        ok(pipe_->AdvanceDevicePosition(n),"TAG advance capture position");
    }
};
}
