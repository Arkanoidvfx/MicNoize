#pragma once
#include "audio.hpp"
#include "tag_endpoint.hpp"
#include "tag_task.hpp"
#include "tag_protocol.hpp"
#include <stdexcept>

namespace mic {
// Same-session IPC. One producer (the existing TAG ownership mutex), one permanent
// driver host. Acknowledged writes add no playback queue or extra reserve.
class TagLink {
    HANDLE mapping_=nullptr;
    GUID host_{},connection_{};
    ULONGLONG sequence_=0;
    bool headphones_=false,poisoned_=false;
public:
    HANDLE mutex=nullptr, request=nullptr, response=nullptr;
    TagPacketV2* packet=nullptr;
    bool running=false;
    unsigned capacity=0,gaps=0;
    struct Lock {
        TagLink& link;
        Lock(TagLink& value,DWORD ms):link(value){if(!link.lock(ms))throw std::runtime_error("TAG host lock timeout");}
        ~Lock(){link.unlock();}
    };
    explicit TagLink(bool create,bool headphones=false,const wchar_t* testNamespace=nullptr):headphones_(headphones) {
        try {
            const std::wstring prefix=testNamespace?testNamespace:(headphones?L"Local\\MicNoize.HeadphoneLink":L"Local\\MicNoize.TagLink");
            mutex=CreateMutexW(nullptr,FALSE,(prefix+L".Lock.v2").c_str());
            request=CreateEventW(nullptr,FALSE,FALSE,(prefix+L".Request.v2").c_str());
            response=CreateEventW(nullptr,FALSE,FALSE,(prefix+L".Response.v2").c_str());
            mapping_=create?CreateFileMappingW(INVALID_HANDLE_VALUE,nullptr,PAGE_READWRITE,0,sizeof(TagPacketV2),(prefix+L".v2").c_str()):
                OpenFileMappingW(FILE_MAP_ALL_ACCESS,FALSE,(prefix+L".v2").c_str());
            if(mapping_) packet=static_cast<TagPacketV2*>(MapViewOfFile(mapping_,FILE_MAP_ALL_ACCESS,0,0,sizeof(TagPacketV2)));
            if(!mutex || !request || !response || !packet) throw std::runtime_error("TAG background host unavailable");
            if(!create) {
                Lock lock(*this,500);host_=packet->host;
                if(!tagHeaderValid(*packet,host_))throw std::runtime_error("TAG host protocol mismatch; update host and UI together");
                if(FAILED(CoCreateGuid(&connection_)))throw std::runtime_error("TAG connection identity failed");
            }
        } catch(...) {close();throw;}
    }
    ~TagLink(){close();}
    TagLink(const TagLink&)=delete;
    TagLink& operator=(const TagLink&)=delete;
    bool lock(DWORD ms) {
        auto r=WaitForSingleObject(mutex,ms);
        return r==WAIT_OBJECT_0 || r==WAIT_ABANDONED;
    }
    void unlock(){ReleaseMutex(mutex);}
    void status() {
        Lock lock(*this,100);
        readStatus();
    }
    bool usable()const{return !poisoned_;}
    int command(unsigned op,const float* samples=nullptr,unsigned frames=0,float* left=nullptr,float* right=nullptr) {
        try {
            if(poisoned_)throw std::runtime_error("TAG connection expired; reconnect processing");
            if(!op || op>4 || frames>(headphones_?8192u:16384u) || (op!=2 && frames) ||
                (frames && (headphones_?(!left || !right):!samples)))throw std::runtime_error("Invalid TAG IPC request");
            const auto deadline=GetTickCount64()+500;
            if(++sequence_==0)throw std::runtime_error("TAG request sequence exhausted");
            {
                Lock lock(*this,100);readStatus();ResetEvent(response);
                packet->command=0;packet->frames=frames;packet->result=-1;
                packet->connection=connection_;packet->request=sequence_;packet->deadline=deadline;
                if(samples && frames)std::copy_n(samples,frames,packet->samples);
                packet->command=op;
            }
            SetEvent(request);
            for(;;) {
                const auto now=GetTickCount64();
                if(now>=deadline || WaitForSingleObject(response,static_cast<DWORD>(deadline-now))!=WAIT_OBJECT_0)
                    throw std::runtime_error("TAG host stopped responding");
                Lock lock(*this,100);readStatus();
                if(packet->ack!=sequence_ || packet->ackHost!=host_ || packet->ackConnection!=connection_)continue;
                if(GetTickCount64()>deadline)throw std::runtime_error("TAG response expired; reconnect processing");
                if(packet->frames!=frames)throw std::runtime_error("TAG response block size mismatch");
                if(!packet->result && headphones_ && frames) {
                    if(!std::all_of(packet->samples,packet->samples+frames*2,[](float v){return std::isfinite(v);}))throw std::runtime_error("Non-finite headphone response");
                    for(unsigned i=0;i<frames;++i){left[i]=packet->samples[i*2];right[i]=packet->samples[i*2+1];}
                }
                return packet->result;
            }
        }catch(...){poisoned_=true;throw;}
    }
private:
    void readStatus() {
        if(!tagHeaderValid(*packet,host_))throw std::runtime_error("TAG host restarted or protocol changed; reconnect processing");
        if(packet->running>1 || packet->capacity>1048576)throw std::runtime_error("Invalid TAG host status");
        running=packet->running!=0;capacity=packet->capacity;gaps=packet->gaps;
    }
    void close() {
        if(packet) UnmapViewOfFile(packet);
        if(mapping_) CloseHandle(mapping_);
        if(mutex) CloseHandle(mutex);
        if(request) CloseHandle(request);
        if(response) CloseHandle(response);
    }
};
// The UI and host ship together; legacy core installation is a migration fallback.
inline bool tagHostInstalled() noexcept {
    try {return tagHostFileCompatible(tagHostPath());}
    catch(...) {return false;}
}
inline HANDLE tagHostProcess() {
    if(tagMaintenancePending())throw std::runtime_error("Mic Noize device maintenance in progress");
    // A client may retain the old mapping after a crash. It is not a liveness check.
    TagEndpointStatus status;
    if(readTagEndpointStatus(status) && tagEndpointHostAlive(status))return nullptr;
    HANDLE owner=OpenMutexW(SYNCHRONIZE,FALSE,L"Local\\MicNoize.TagHost");
    if(owner){CloseHandle(owner);return nullptr;} // Includes an older host awaiting coordinated update.
    if(!tagHostInstalled())throw std::runtime_error("Обновите фоновый компонент Mic Noize: несовместимая версия хоста");
    try {
        configureTagTask();runTagTask();setTagTaskWarning({});return nullptr;
    } catch(const std::exception& e) {
        setTagTaskWarning(std::string("Планировщик недоступен: после падения при закрытом UI хост не восстановится. ")+e.what());
    }
    const auto exe=tagHostPath();
    std::wstring command=L"\""+exe.wstring()+L"\" --direct";
    STARTUPINFOW startup{};startup.cb=sizeof(startup);startup.dwFlags=STARTF_USESHOWWINDOW;startup.wShowWindow=SW_HIDE;
    PROCESS_INFORMATION process{};
    if(!CreateProcessW(exe.c_str(),command.data(),nullptr,nullptr,FALSE,CREATE_NO_WINDOW,nullptr,exe.parent_path().c_str(),&startup,&process))
        throw std::runtime_error("Не удалось запустить фоновый процесс виртуального микрофона");
    CloseHandle(process.hThread);return process.hProcess;
}
inline void ensureTagHost() {
    HANDLE process=tagHostProcess();
    struct CloseProcess {HANDLE& process;~CloseProcess(){if(process)CloseHandle(process);}} closeProcess{process};
    // The mapping is published after TAG opens; wait for protocol initialization too.
    for(unsigned i=0;i<200;++i) {
        TagEndpointStatus endpoint;
        const bool published=readTagEndpointStatus(endpoint);
        if(published && (endpoint.audioVersion!=tagProtocolVersion || endpoint.hostBuild!=tagHostBuild))throw std::runtime_error("TAG host protocol mismatch; update host and UI together");
        if(published && endpoint.ready)return;
        // A second launcher can exit because another host is still initializing.
        if(process && WaitForSingleObject(process,10)==WAIT_OBJECT_0) {CloseHandle(process);process=nullptr;}
        if(!process) Sleep(10);
    }
    TagEndpointStatus endpoint;
    if(readTagEndpointStatus(endpoint) && endpoint.error[0])throw std::runtime_error(endpoint.error);
    throw std::runtime_error("TAG host initializing or waiting for driver; see results/tag-host.log");
}
// Read side of TagBufferV1, absent with an older host; tries to open it at most once a second.
// One per TAG session: a restarted host means a reconnect, and a new view of its new mapping.
class TagBufferView {
    HANDLE mapping_=nullptr;
    const TagBufferV1* view_=nullptr;
    ULONGLONG retryAt_=0;
public:
    TagBufferView()=default;
    TagBufferView(const TagBufferView&)=delete;
    TagBufferView& operator=(const TagBufferView&)=delete;
    ~TagBufferView(){if(view_)UnmapViewOfFile(view_);if(mapping_)CloseHandle(mapping_);}
    bool read(TagBufferV1& out) {
        if(!view_) {
            const auto now=GetTickCount64();
            if(now<retryAt_)return false;
            retryAt_=now+1000;
            if(!mapping_)mapping_=OpenFileMappingW(FILE_MAP_READ,FALSE,tagBufferMapping);
            if(!mapping_)return false;
            view_=static_cast<const TagBufferV1*>(MapViewOfFile(mapping_,FILE_MAP_READ,0,0,sizeof(TagBufferV1)));
            if(!view_)return false;
        }
        return readTagBuffer(*view_,out);
    }
};
class TagClient {
    TagLink link_;
    bool rejected_=false;
public:
    TagClient():link_(false){if(link_.command(1))throw std::runtime_error("TAG connection rejected");}
    ~TagClient(){if(link_.usable())try{link_.command(3);}catch(...){}}
    void handleEvent(){link_.status();}
    bool running() const{return link_.running;}
    unsigned capacity() const{return link_.capacity;}
    unsigned driverGaps() const{return link_.gaps;}
    // False: the host's 50 ms producer watchdog dropped us and already covered the gap with
    // silence, so this block (which carries the clock debt of the stall) is discarded and the
    // producer re-registers. Two consecutive rejections are a real protocol error.
    bool write(const float* samples,unsigned frames) {
        if(frames>16384) throw std::runtime_error("TAG IPC block exceeds allocation");
        if(link_.command(2,samples,frames)==0) {rejected_=false;return true;}
        if(rejected_) throw std::runtime_error("TAG host rejected audio; restart processing");
        rejected_=true;if(link_.command(1))throw std::runtime_error("TAG reconnection rejected");return false;
    }
};
class HeadphoneClient {
    TagLink link_{false,true};
    void command(unsigned op,float* left=nullptr,float* right=nullptr,unsigned frames=0) {
        if(link_.command(op,nullptr,frames,left,right))throw std::runtime_error("Headphone host rejected request; see results/tag-headphones.log");
    }
public:
    HeadphoneClient(){command(1);}
    ~HeadphoneClient(){if(link_.usable())try{command(3);}catch(...){}}
    void handleEvent(){command(4);}
    bool running() const{return link_.running;}
    unsigned capacity() const{return link_.capacity;}
    void read(float* left,float* right,unsigned n){command(2,left,right,n);}
};
}
