#include "tag.hpp"
#include "tag_link.hpp"
#include <avrt.h>
#include <fstream>
#include <chrono>
#include <memory>
#include <string_view>

namespace {
struct Handle {HANDLE h=nullptr;~Handle(){if(h)CloseHandle(h);}};
void logHost(const char* message) noexcept {
    try {
        const auto dir=mic::projectRoot()/L"results";std::filesystem::create_directories(dir);
        const auto file=dir/L"tag-host.log";
        if(std::filesystem::exists(file) && std::filesystem::file_size(file)>256*1024) {
            std::error_code ec;std::filesystem::remove(dir/L"tag-host.previous.log",ec);std::filesystem::rename(file,dir/L"tag-host.previous.log",ec);
        }
        SYSTEMTIME time{};GetSystemTime(&time);char stamp[40];sprintf_s(stamp,"%04u-%02u-%02uT%02u:%02u:%02uZ ",time.wYear,time.wMonth,time.wDay,time.wHour,time.wMinute,time.wSecond);
        std::ofstream(file,std::ios::app)<<stamp<<message<<'\n';
    }catch(...){}
}
int superviseHost() {
    Handle singleton{CreateMutexW(nullptr,FALSE,L"Local\\MicNoize.TagSupervisor.v2")};
    if(!singleton.h)throw std::runtime_error("Supervisor mutex unavailable");
    if(GetLastError()==ERROR_ALREADY_EXISTS)return 0;
    const auto generation=mic::scheduledTagRecovery();if(generation==GUID_NULL)return 0;
    if(!mic::publishTagOwner(generation,true))return 0;
    Handle stop{CreateEventW(nullptr,TRUE,FALSE,L"Local\\MicNoize.TagHost.Stop.v2")};
    Handle heartbeat{CreateEventW(nullptr,FALSE,FALSE,L"Local\\MicNoize.TagHost.Heartbeat.v2")};
    Handle pulse{CreateEventW(nullptr,FALSE,FALSE,L"Local\\MicNoize.TagSupervisor.Heartbeat.v1")};
    if(!stop.h || !heartbeat.h || !pulse.h)throw std::runtime_error("Supervisor events unavailable");
    const auto waitRetry=[&](unsigned milliseconds) {
        const auto start=mic::tagAwakeMilliseconds();
        do {
            SetEvent(pulse.h);
            if(WaitForSingleObject(stop.h,1000)!=WAIT_TIMEOUT)return false;
        }while(mic::tagAwakeMilliseconds()-start<milliseconds);
        return true;
    };
    SetEvent(pulse.h);
    Handle process{mic::tagHostProcess(PROCESS_TERMINATE,true)};
    if(process.h) {
        if(WaitForSingleObject(stop.h,0)==WAIT_OBJECT_0 || !mic::tagRecoveryCurrent(generation))return 0;
        logHost("Supervisor adopted the surviving verified worker");
    } else ResetEvent(stop.h);
    unsigned startupAttempts=0;
    for(;;) {
        if(WaitForSingleObject(stop.h,0)==WAIT_OBJECT_0 || !mic::tagRecoveryCurrent(generation))return 0;
        if(!process.h) {
            mic::tagRecoveryPhase(mic::TagDeviceState::Starting,generation);
            wchar_t path[32768]{};const auto length=GetModuleFileNameW(nullptr,path,std::size(path));
            if(!length || length==std::size(path))throw std::runtime_error("Supervisor executable path unavailable");
            wchar_t token[40];if(!StringFromGUID2(generation,token,std::size(token)))throw std::runtime_error("Invalid worker generation");
            std::wstring command=L"\""+std::wstring(path)+L"\" --worker="+token;
            STARTUPINFOW startup{};startup.cb=sizeof(startup);startup.dwFlags=STARTF_USESHOWWINDOW;startup.wShowWindow=SW_HIDE;
            PROCESS_INFORMATION child{};ResetEvent(heartbeat.h);
            if(!CreateProcessW(path,command.data(),nullptr,nullptr,FALSE,CREATE_NO_WINDOW,nullptr,nullptr,&startup,&child))throw std::runtime_error("Create supervised host failed");
            process.h=child.hProcess;CloseHandle(child.hThread);
        }
        auto lastPulse=mic::tagAwakeMilliseconds();DWORD code=1;
        HANDLE events[]={stop.h,process.h,heartbeat.h};
        for(;;) {
            SetEvent(pulse.h);
            const auto wait=WaitForMultipleObjects(3,events,FALSE,1000);
            if(wait==WAIT_OBJECT_0) {
                // Cooperative stop first; forced cleanup is limited to the verified worker.
                if(WaitForSingleObject(process.h,5000)!=WAIT_OBJECT_0)TerminateProcess(process.h,0);
                return 0;
            }
            if(wait==WAIT_OBJECT_0+1){GetExitCodeProcess(process.h,&code);break;}
            if(wait==WAIT_OBJECT_0+2)lastPulse=mic::tagAwakeMilliseconds();
            if(wait==WAIT_FAILED || mic::tagAwakeMilliseconds()-lastPulse>15000) {
                logHost("Supervised host heartbeat stalled; terminating owned child");TerminateProcess(process.h,1);WaitForSingleObject(process.h,5000);break;
            }
        }
        CloseHandle(process.h);process.h=nullptr;
        if(code==0){mic::cancelTagRecovery(generation);return 0;}
        if(code==3) {
            // A prior worker can acquire ownership before publishing its endpoint.
            for(unsigned attempt=0;attempt<15 && !process.h;++attempt) {
                if(!waitRetry(1000))return 0;
                process.h=mic::tagHostProcess(PROCESS_TERMINATE,true);
            }
            if(process.h){logHost("Supervisor adopted the initializing verified worker");continue;}
            mic::tagRecoveryPhase(mic::TagDeviceState::UserAction,generation);
            logHost("TAG ownership conflict; no foreign process was stopped");
            mic::cancelTagRecovery(generation);return 0;
        }
        if(code==2) {
            mic::tagRecoveryPhase(mic::TagDeviceState::WaitingDriver,generation);
            constexpr unsigned delay[]={2,5,15,30,60};
            const auto seconds=delay[std::min(startupAttempts++,4u)];
            logHost("Waiting for TAG driver/runtime; retrying with startup backoff");
            if(!waitRetry(seconds*1000))return 0;
            continue;
        }
        if(!mic::takeTagRecovery(generation)){mic::tagRecoveryPhase(mic::TagDeviceState::UserAction,generation);logHost("Host recovery exhausted after three retries; open Mic Noize to retry");return 0;}
        mic::tagRecoveryPhase(mic::TagDeviceState::Recovering,generation);
        logHost("Host crashed; supervised restart in 60 seconds");
        if(!waitRetry(60000))return 0;
    }
}
}

int WINAPI wWinMain(HINSTANCE,HINSTANCE,PWSTR arguments,int) {
    std::wstring mode;
    GUID workerGeneration{};
    try{
        mode=mic::tagHostMode(arguments);
        if(mode.starts_with(L"--worker=")) {
            if(FAILED(CLSIDFromString(mode.substr(9).c_str(),&workerGeneration)) || !mic::tagRecoveryCurrent(workerGeneration))return 0;
            mode=L"--worker";
        } else if(mode==L"--worker")throw std::runtime_error("Host worker requires a current recovery generation");
    }catch(const std::exception& error){logHost(error.what());return 2;}
    try {
        if(mode!=L"--stop" && mode!=L"--task-remove" && mic::tagMaintenancePending()) {
            if(mode==L"--scheduled" || mode==L"--worker")return 0;
            logHost("Mic Noize device maintenance in progress");return 1;
        }
        if(mode==L"--task-enable" || mode==L"--task-disable") {mic::configureTagTask(mode==L"--task-enable");return 0;}
        if(mode==L"--task-remove") {mic::removeTagTask();return 0;}
        if(mode==L"--task-start" || mode.empty()) {mic::configureTagTask();mic::runTagTask();return 0;}
        if(mode==L"--recover-host") {mic::recoverTagTask();return 0;}
        if(mode==L"--scheduled")return superviseHost();
        if(mode==L"--stop") {
            mic::stopTagHost();return 0;
        }
        if(mode!=L"--worker" && mode!=L"--direct")return 2;
    }catch(const std::exception& error){logHost(error.what());if(!mode.empty())return 1;logHost("Scheduler unavailable; starting directly without closed-UI crash recovery");}
    // The signed driver has one owner across all Windows sessions. Never displace it.
    Handle driverOwner{CreateMutexW(nullptr,FALSE,L"Global\\MicNoize.TAG.Driver")};
    if(!driverOwner.h || GetLastError()==ERROR_ALREADY_EXISTS){logHost("TAG driver belongs to another host/session");return 3;}
    HANDLE owner=CreateMutexW(nullptr,FALSE,L"Local\\MicNoize.TagHost");
    if(!owner) return 1;
    if(GetLastError()==ERROR_ALREADY_EXISTS){CloseHandle(owner);return 3;}
    int result=0;
    try {
        Handle managed{mode==L"--worker"?CreateMutexW(nullptr,FALSE,L"Local\\MicNoize.TagHost.Scheduled.v2"):nullptr};
        Handle stop{CreateEventW(nullptr,TRUE,FALSE,L"Local\\MicNoize.TagHost.Stop.v2")};
        if(!stop.h || (mode==L"--worker" && !managed.h))throw std::runtime_error("Host lifetime handles unavailable");
        if(mode!=L"--worker")ResetEvent(stop.h);
        if(WaitForSingleObject(stop.h,0)==WAIT_OBJECT_0)return 0;
        if(mode==L"--worker" && !mic::publishTagOwner(workerGeneration,false))return 0;
        Handle heartbeat{mode==L"--worker"?CreateEventW(nullptr,FALSE,FALSE,L"Local\\MicNoize.TagHost.Heartbeat.v2"):nullptr};
        if(mode==L"--worker" && !heartbeat.h)throw std::runtime_error("Host supervisor heartbeat unavailable");
        Handle event{CreateEventW(nullptr,FALSE,FALSE,nullptr)};
        Handle timer{CreateWaitableTimerExW(nullptr,nullptr,CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,TIMER_ALL_ACCESS)};
        if(!event.h || !timer.h) throw std::runtime_error("TAG host event/timer creation failed");
        mic::TagOutput tag(mic::projectRoot()/L"vendor/tag-2.0.0.1903-demo",event.h);
        GUID hostId{};if(FAILED(CoCreateGuid(&hostId)))throw std::runtime_error("Host run identity unavailable");
        mic::TagLink link(true);
        mic::TagLink headphones(true,true);
        Handle headphoneEvent{CreateEventW(nullptr,FALSE,FALSE,nullptr)};
        if(!headphoneEvent.h)throw std::runtime_error("Headphone event failed");
        if(!headphones.lock(500))throw std::runtime_error("Headphone initialization lock");
        *headphones.packet={};headphones.packet->host=hostId;headphones.unlock();
        std::unique_ptr<mic::TagOutput> headphonePipe;
        std::array<float,8192> left{},right{};
        auto headphoneSeen=std::chrono::steady_clock::now();
        if(!link.lock(500)) throw std::runtime_error("TAG host initialization lock");
        *link.packet={};link.packet->host=hostId;link.unlock();
        mic::TagEndpointGuard endpoint(tag.driverInterface(),tag.ksName(),tag.lineId(),hostId);
        mic::TagSession microphoneSession{hostId},headphoneSession{hostId};
        std::atomic<ULONGLONG> dispatchPulse=mic::tagAwakeMilliseconds();
        std::atomic<bool> headphoneFailed=false;
        std::jthread watchdog([&](std::stop_token token){
            bool previousHeadphoneFailure=false;
            bool missing=false;ULONGLONG retryRecoveryAt=0;
            while(!token.stop_requested() && WaitForSingleObject(stop.h,1000)==WAIT_TIMEOUT) {
                const bool failed=headphoneFailed;
                if(failed!=previousHeadphoneFailure){logHost(failed?"Headphone client failed; microphone host remains active":"Headphone client recovered");previousHeadphoneFailure=failed;}
                if(mic::tagAwakeMilliseconds()-dispatchPulse.load()>15000 || !endpoint.responsive()) {
                    logHost("Host dispatcher or endpoint controller hung; exiting for scheduled recovery");ExitProcess(1);
                }
                if(workerGeneration!=GUID_NULL) {
                    Handle supervisor{OpenMutexW(SYNCHRONIZE,FALSE,L"Local\\MicNoize.TagSupervisor.v2")};
                    if(supervisor.h){missing=false;retryRecoveryAt=0;continue;}
                    if(!missing){missing=true;logHost("Supervisor lost; worker stays active; recovery in 60 awake seconds");}
                    if(mic::tagAwakeMilliseconds()<retryRecoveryAt)continue;
                    try {
                        if(WaitForSingleObject(stop.h,0)==WAIT_OBJECT_0 || mic::tagMaintenancePending())continue;
                        if(!mic::recoverTagTask()){retryRecoveryAt=ULLONG_MAX;logHost("Background recovery stopped; waiting for explicit refresh or Stop");}
                    }catch(const std::exception& error){logHost(error.what());retryRecoveryAt=mic::tagAwakeMilliseconds()+60000;}
                }
            }
        });
        DWORD task=0;HANDLE mmcss=AvSetMmThreadCharacteristicsW(L"Audio",&task);
        struct Priority {HANDLE h;~Priority(){if(h)AvRevertMmThreadCharacteristics(h);}} priority{mmcss};
        std::array<float,16384> silence{};
        // Capture buffer diagnostics for the engine's report; audio never depends on them.
        Handle bufferMapping{CreateFileMappingW(INVALID_HANDLE_VALUE,nullptr,PAGE_READWRITE,0,sizeof(mic::TagBufferV1),mic::tagBufferMapping)};
        void* bufferMemory=bufferMapping.h?MapViewOfFile(bufferMapping.h,FILE_MAP_ALL_ACCESS,0,0,sizeof(mic::TagBufferV1)):nullptr;
        struct Unmap {void* p;~Unmap(){if(p)UnmapViewOfFile(p);}} unmapBuffer{bufferMemory};
        auto* bufferView=bufferMemory?new(bufferMemory) mic::TagBufferV1{}:nullptr;
        if(!bufferView)logHost("TAG buffer diagnostics unavailable");
        bool connected=false,producing=false,wasRunning=false,retryLock=false;
        auto last=std::chrono::steady_clock::now(),lastProducer=last;
        mic::TagClock clock;
        HANDLE events[]={event.h,link.request,timer.h,headphones.request,headphoneEvent.h,stop.h};
        for(;;) {
            dispatchPulse=mic::tagAwakeMilliseconds();
            if(heartbeat.h)SetEvent(heartbeat.h);
            const auto wait=WaitForMultipleObjects(6,events,FALSE,retryLock?2:1000);
            if(wait==WAIT_FAILED) throw std::runtime_error("TAG host wait failed");
            if(WaitForSingleObject(stop.h,0)==WAIT_OBJECT_0)break;
            tag.handleEvent();
            const auto now=std::chrono::steady_clock::now();
            if(now-last>std::chrono::seconds(2)) {connected=false;producing=false;clock={};last=now;headphonePipe.reset();}
            const bool headphoneRetry=!headphones.lock(0);
            if(!headphoneRetry) {
                auto& p=*headphones.packet;const unsigned op=p.command;p.command=0;
                if(op && !headphoneSession.accept(p,op,GetTickCount64(),true))p.result=1;
                else try {
                    if(op)headphoneSeen=now;
                    if(op==1 && !headphonePipe){headphonePipe=std::make_unique<mic::TagOutput>(mic::projectRoot()/L"vendor/tag-2.0.0.1903-demo",headphoneEvent.h,false,&tag);headphoneFailed=false;}
                    if(op==3 || (headphonePipe && now-headphoneSeen>std::chrono::seconds(2)))headphonePipe.reset();
                    if(headphonePipe)headphonePipe->handleEvent();
                    p.running=headphonePipe && headphonePipe->running();p.capacity=headphonePipe?headphonePipe->capacity():0;p.result=0;
                    if(op==2){
                        if(!headphonePipe || p.frames>8192 || (p.running && p.frames>p.capacity/2))throw std::runtime_error("Invalid headphone read size");
                        if(p.running)headphonePipe->read(left.data(),right.data(),p.frames);
                        else {left.fill(0);right.fill(0);}
                        for(unsigned i=0;i<p.frames;++i){p.samples[i*2]=left[i];p.samples[i*2+1]=right[i];}
                    }else if(op>4)throw std::runtime_error("Invalid headphone command");
                }catch(const std::exception&){
                    p.result=1;p.running=0;headphonePipe.reset();
                    headphoneFailed=true;
                }
                if(op)headphoneSession.reply(p);
                headphones.unlock();if(op)SetEvent(headphones.response);
            }
            const bool running=tag.running();
            if(running!=wasRunning){
                clock={};last=now;wasRunning=running;producing=false;
                if(running) {
                    LARGE_INTEGER due{};due.QuadPart=-20000;
                    if(!SetWaitableTimer(timer.h,&due,2,nullptr,nullptr,FALSE)) throw std::runtime_error("TAG host timer start failed");
                } else CancelWaitableTimer(timer.h);
            }
            unsigned command=0;
            // A suspended/crashed UI must never hold the driver host hostage.
            retryLock=!link.lock(0);
            if(!retryLock) {
                auto& p=*link.packet;
                p.running=running && endpoint.ready();p.capacity=tag.capacity();p.gaps=tag.driverGaps();
                command=p.command;
                if(command) {
                p.result=0;p.command=0;
                if(!microphoneSession.accept(p,command,GetTickCount64()))p.result=1;
                else if(command==1){connected=true;producing=false;lastProducer=now;}
                else if(command==3){connected=false;producing=false;clock={};}
                else if(command==2 && connected && p.frames<=16384) {
                    const bool finite=std::all_of(p.samples,p.samples+p.frames,[](float v){return std::isfinite(v);});
                    if(!finite || !endpoint.ready() || (running && p.frames>tag.capacity()/2)) p.result=1;
                    else if(running) {
                        // A synchronous handoff: samples are consumed before the producer
                        // returns. No second audio queue, no old tail after Stop/crash.
                        try {tag.write(p.samples,p.frames);}
                        catch(...) {p.result=1;}
                    }
                    lastProducer=now;last=now;clock={};producing=running && p.result==0;
                } else if(command!=4)p.result=1;
                microphoneSession.reply(p);
                }
                link.unlock();
            }
            if(command) SetEvent(link.response);
            retryLock=retryLock || headphoneRetry;
            if(producing && (!endpoint.ready() || now-lastProducer>std::chrono::milliseconds(50))) {
                producing=false;connected=false;clock={};last=now;
            }
            if(running && !producing) {
                const auto frames=clock.take(std::chrono::duration<double>(now-last).count(),0,std::min(tag.capacity()/2,16384u));
                if(frames) tag.write(silence.data(),frames);
            }
            last=now;
            if(bufferView)mic::publishTagBuffer(*bufferView,tag.bufferState());
        }
    } catch(const std::exception& e) {
        logHost(e.what());
        result=mic::tagTransientStartup(e.what())?2:1;
    }
    CloseHandle(owner);return result;
}
