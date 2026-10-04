#include "tag_task.hpp"
#include "audio.hpp"
#include "tag_endpoint.hpp"
#include "tag.hpp"
#include <taskschd.h>
#include <sddl.h>
#include <wrl/client.h>
#include <fstream>
#include <shellapi.h>
#include <tlhelp32.h>

namespace mic {
namespace {
using Microsoft::WRL::ComPtr;
constexpr wchar_t runKey[]=L"Software\\Microsoft\\Windows\\CurrentVersion\\Run";
constexpr wchar_t author[]=L"Mic Noize background host v2";
constexpr wchar_t recoveryKey[]=L"Software\\MicNoize\\TAG\\Recovery";
std::filesystem::path legacyHostPath(){return (projectRoot()/L"bin"/L"mic_tag_host.exe").lexically_normal().make_preferred();}
std::mutex warningMutex;
std::string warningText;
void checkTask(HRESULT hr,const char* what) {
    if(FAILED(hr)){char error[200];sprintf_s(error,"%s: HRESULT 0x%08lX",what,static_cast<unsigned long>(hr));throw std::runtime_error(error);}
}
struct Bstr {
    BSTR value=nullptr;
    Bstr()=default;
    explicit Bstr(const std::wstring& text):value(SysAllocStringLen(text.data(),static_cast<UINT>(text.size()))){if(!value)throw std::bad_alloc();}
    ~Bstr(){SysFreeString(value);}
    Bstr(const Bstr&)=delete;
    operator BSTR()const{return value;}
};
struct Apartment {
    HRESULT hr=CoInitializeEx(nullptr,COINIT_MULTITHREADED);
    Apartment(){if(hr!=RPC_E_CHANGED_MODE)checkTask(hr,"Task Scheduler COM");}
    ~Apartment(){if(SUCCEEDED(hr))CoUninitialize();}
};
struct RecoveryRecord {
    unsigned version=1;
    LUID logon{};
    DWORD session=0;
    TagRecoveryBudget budget{};
};
struct RecoveryPhase {GUID generation{};TagDeviceState state=TagDeviceState::Starting;};
struct RecoveryOwner {GUID generation{};DWORD pid=0;FILETIME started{};unsigned build=tagHostBuild;};
struct RecoveryStore {
    HANDLE lock=CreateMutexW(nullptr,FALSE,L"Global\\MicNoize.TagRecovery.v1");
    HKEY key=nullptr;
    RecoveryRecord record{};
    LUID logon{};DWORD session=0;
    explicit RecoveryStore(DWORD timeout=5000) {
        const auto wait=lock?WaitForSingleObject(lock,timeout):WAIT_FAILED;
        if(wait!=WAIT_OBJECT_0 && wait!=WAIT_ABANDONED){if(lock)CloseHandle(lock);throw std::runtime_error("Host recovery state busy");}
        try {
            HANDLE token=nullptr;
            if(!OpenProcessToken(GetCurrentProcess(),TOKEN_QUERY,&token))throw std::runtime_error("Recovery logon token unavailable");
            struct Close{HANDLE h;~Close(){CloseHandle(h);}} close{token};
            TOKEN_STATISTICS stats{};DWORD bytes=0;
            if(!GetTokenInformation(token,TokenStatistics,&stats,sizeof(stats),&bytes) || !ProcessIdToSessionId(GetCurrentProcessId(),&session) || !session)
                throw std::runtime_error("Recovery requires an interactive logon");
            logon=stats.AuthenticationId;
            checkTask(HRESULT_FROM_WIN32(RegCreateKeyExW(HKEY_CURRENT_USER,recoveryKey,0,nullptr,0,KEY_QUERY_VALUE|KEY_SET_VALUE,nullptr,&key,nullptr)),"Open recovery state");
            DWORD type=0;bytes=sizeof(record);
            const auto result=RegQueryValueExW(key,L"State",nullptr,&type,reinterpret_cast<BYTE*>(&record),&bytes);
            if(result==ERROR_FILE_NOT_FOUND)record={};
            else if(result!=ERROR_SUCCESS || type!=REG_BINARY || bytes!=sizeof(record) || record.version!=1 || record.budget.active>1 || record.budget.used>3 || (record.budget.active && record.budget.generation==GUID_NULL))
                throw std::runtime_error("Invalid persisted host recovery state");
        }catch(...){if(key)RegCloseKey(key);ReleaseMutex(lock);CloseHandle(lock);throw;}
    }
    ~RecoveryStore(){RegCloseKey(key);ReleaseMutex(lock);CloseHandle(lock);}
    bool ours()const{return record.session==session && record.logon.LowPart==logon.LowPart && record.logon.HighPart==logon.HighPart;}
    void save(){checkTask(HRESULT_FROM_WIN32(RegSetValueExW(key,L"State",0,REG_BINARY,reinterpret_cast<const BYTE*>(&record),sizeof(record))),"Save host recovery state");}
    void begin(){record={};record.logon=logon;record.session=session;checkTask(CoCreateGuid(&record.budget.generation),"Recovery generation");record.budget.active=1;save();}
};
HANDLE openHostIdentity(DWORD pid,const FILETIME& created,DWORD access) {
    HANDLE process=OpenProcess(access|SYNCHRONIZE|PROCESS_QUERY_LIMITED_INFORMATION,FALSE,pid);
    if(!process){if(GetLastError()==ERROR_INVALID_PARAMETER)return nullptr;throw std::runtime_error("Cannot verify host process identity");}
    struct Close {HANDLE h;~Close(){if(h)CloseHandle(h);}} close{process};
    if(WaitForSingleObject(process,0)==WAIT_OBJECT_0)return nullptr;
    wchar_t path[32768]{};DWORD size=std::size(path),session=0,ours=0;FILETIME started{},ended{},kernel{},user{};
    if(!QueryFullProcessImageNameW(process,0,path,&size) || _wcsicmp(path,tagHostPath().c_str()) ||
       !GetProcessTimes(process,&started,&ended,&kernel,&user) || CompareFileTime(&started,&created) ||
       !ProcessIdToSessionId(pid,&session) || !ProcessIdToSessionId(GetCurrentProcessId(),&ours) || session!=ours)
        throw std::runtime_error("Host process identity changed; no process was stopped");
    close.h=nullptr;return process;
}
HANDLE recoveryOwner(RecoveryStore& state,bool supervisor,DWORD access) {
    RecoveryOwner owner;DWORD bytes=sizeof(owner),type=0;
    const auto result=RegQueryValueExW(state.key,supervisor?L"Supervisor":L"Worker",nullptr,&type,reinterpret_cast<BYTE*>(&owner),&bytes);
    if(result==ERROR_FILE_NOT_FOUND)return nullptr;
    checkTask(HRESULT_FROM_WIN32(result),"Read host owner");
    if(bytes!=sizeof(owner) || type!=REG_BINARY || !owner.pid)throw std::runtime_error("Invalid persisted host owner");
    if(!state.ours() || !state.record.budget.current(owner.generation) || owner.build!=tagHostBuild)return nullptr;
    return openHostIdentity(owner.pid,owner.started,access);
}
std::wstring userSid() {
    HANDLE token=nullptr;
    if(!OpenProcessToken(GetCurrentProcess(),TOKEN_QUERY,&token))checkTask(HRESULT_FROM_WIN32(GetLastError()),"Task user token");
    struct Close {HANDLE h;~Close(){CloseHandle(h);}} close{token};
    DWORD bytes=0;GetTokenInformation(token,TokenUser,nullptr,0,&bytes);
    if(!bytes || bytes>65536)throw std::runtime_error("Invalid task user token size");
    std::vector<BYTE> buffer(bytes);
    if(!GetTokenInformation(token,TokenUser,buffer.data(),bytes,&bytes))checkTask(HRESULT_FROM_WIN32(GetLastError()),"Task user identity");
    LPWSTR raw=nullptr;
    if(!ConvertSidToStringSidW(reinterpret_cast<TOKEN_USER*>(buffer.data())->User.Sid,&raw))checkTask(HRESULT_FROM_WIN32(GetLastError()),"Task user SID");
    std::wstring sid=raw;LocalFree(raw);return sid;
}
bool legacyLogin() {
    for(const auto name:{L"MicNoize.TagHost",L"MicNoiseReducer.TagHost"}) {
        DWORD bytes=0;
        if(RegGetValueW(HKEY_CURRENT_USER,runKey,name,RRF_RT_REG_SZ,nullptr,nullptr,&bytes)==ERROR_SUCCESS)return true;
    }
    return false;
}
bool sameUser(const wchar_t* account,const std::wstring& expected) {
    if(!account)return false;
    if(!_wcsicmp(account,expected.c_str()))return true;
    std::wstring qualified=account;
    // A bare local account can collide with the computer/domain name.
    if(qualified.find_first_of(L"\\@") == std::wstring::npos) {
        wchar_t computer[MAX_COMPUTERNAME_LENGTH+1]{};DWORD count=std::size(computer);
        if(!GetComputerNameW(computer,&count))return false;
        qualified=std::wstring(computer)+L"\\"+qualified;
    }
    BYTE sid[SECURITY_MAX_SID_SIZE]{};DWORD bytes=sizeof(sid),domainSize=256;wchar_t domain[256]{};SID_NAME_USE type;
    if(!LookupAccountNameW(nullptr,qualified.c_str(),sid,&bytes,domain,&domainSize,&type))return false;
    LPWSTR text=nullptr;if(!ConvertSidToStringSidW(sid,&text))return false;
    const bool equal=!_wcsicmp(text,expected.c_str());LocalFree(text);return equal;
}
void removeLegacy() {
    HKEY key=nullptr;const auto opened=RegOpenKeyExW(HKEY_CURRENT_USER,runKey,0,KEY_SET_VALUE,&key);
    if(opened==ERROR_FILE_NOT_FOUND)return;
    checkTask(HRESULT_FROM_WIN32(opened),"Open old host autostart");
    struct Close {HKEY key;~Close(){RegCloseKey(key);}} close{key};
    for(const auto name:{L"MicNoize.TagHost",L"MicNoiseReducer.TagHost"}) {
        const auto hr=RegDeleteValueW(key,name);
        if(hr!=ERROR_FILE_NOT_FOUND)checkTask(HRESULT_FROM_WIN32(hr),"Remove old host autostart");
    }
}
std::wstring xmlEscape(const std::wstring& value) {
    std::wstring result;
    for(auto c:value)switch(c){case L'&':result+=L"&amp;";break;case L'<':result+=L"&lt;";break;case L'>':result+=L"&gt;";break;case L'\"':result+=L"&quot;";break;default:result+=c;}
    return result;
}
struct Task {
    struct Lock {
        HANDLE handle=CreateMutexW(nullptr,FALSE,L"Local\\MicNoize.TagTask.Configure");
        Lock(){const auto result=handle?WaitForSingleObject(handle,5000):WAIT_FAILED;if(result!=WAIT_OBJECT_0 && result!=WAIT_ABANDONED){if(handle)CloseHandle(handle);throw std::runtime_error("Host task configuration busy");}}
        ~Lock(){ReleaseMutex(handle);CloseHandle(handle);}
    } lock;
    Apartment apartment;
    std::wstring sid=userSid(),name=L"MicNoize.TagHost."+sid;
    ComPtr<ITaskService> service;
    ComPtr<ITaskFolder> folder;
    ComPtr<IRegisteredTask> task;
    Task() {
        checkTask(CoCreateInstance(CLSID_TaskScheduler,nullptr,CLSCTX_INPROC_SERVER,IID_PPV_ARGS(&service)),"Task Scheduler");
        VARIANT empty{};checkTask(service->Connect(empty,empty,empty,empty),"Connect Task Scheduler");
        checkTask(service->GetFolder(Bstr(L"\\"),&folder),"Task Scheduler folder");
        const auto hr=folder->GetTask(Bstr(name),&task);
        if(hr!=HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND))checkTask(hr,"Read host task");
        verifyOwner(task.Get());
    }
    void verifyOwner(IRegisteredTask* registered) {
        if(registered) {
            ComPtr<ITaskDefinition> def;ComPtr<IRegistrationInfo> info;ComPtr<IPrincipal> principal;Bstr owner,user;
            checkTask(registered->get_Definition(&def),"Read task definition");
            checkTask(def->get_RegistrationInfo(&info),"Read task owner");checkTask(info->get_Author(&owner.value),"Read task author");
            checkTask(def->get_Principal(&principal),"Read task principal");checkTask(principal->get_UserId(&user.value),"Read task user");
            if(!owner.value || wcscmp(owner.value,author) || !sameUser(user.value,sid))throw std::runtime_error("Host task identity conflict");
        }
    }
    ComPtr<IRegisteredTask> recovery() {
        ComPtr<IRegisteredTask> result;
        const auto hr=folder->GetTask(Bstr(name+L".Recovery"),&result);
        if(hr!=HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND))checkTask(hr,"Read recovery task");
        verifyOwner(result.Get());return result;
    }
    bool login() {
        if(!task)return legacyLogin();
        ComPtr<ITaskDefinition> def;ComPtr<ITriggerCollection> triggers;LONG count=0;
        checkTask(task->get_Definition(&def),"Read task definition");checkTask(def->get_Triggers(&triggers),"Read host login triggers");
        checkTask(triggers->get_Count(&count),"Count host login triggers");
        for(LONG i=1;i<=count;++i){ComPtr<ITrigger> trigger;TASK_TRIGGER_TYPE2 type;VARIANT_BOOL enabled;
            checkTask(triggers->get_Item(i,&trigger),"Read host trigger");checkTask(trigger->get_Type(&type),"Read trigger type");checkTask(trigger->get_Enabled(&enabled),"Read trigger state");
            if(type==TASK_TRIGGER_LOGON && enabled==VARIANT_TRUE)return true;
        }
        return false;
    }
};
void armRecovery(Task& current) {
    current.recovery(); // Refuse to replace another author's task.
    const auto exe=tagHostPath();const auto user=xmlEscape(current.sid);
    SYSTEMTIME time{};GetLocalTime(&time);wchar_t start[32];
    swprintf_s(start,L"%04u-%02u-%02uT%02u:%02u:%02u",time.wYear,time.wMonth,time.wDay,time.wHour,time.wMinute,time.wSecond);
    const std::wstring xml=L"<?xml version=\"1.0\" encoding=\"UTF-16\"?><Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">"
        L"<RegistrationInfo><Author>"+std::wstring(author)+L"</Author><Description>Recover a stopped Mic Noize supervisor during this logon</Description></RegistrationInfo>"
        L"<Triggers><TimeTrigger><Repetition><Interval>PT1M</Interval><StopAtDurationEnd>false</StopAtDurationEnd></Repetition><StartBoundary>"+start+L"</StartBoundary><Enabled>true</Enabled></TimeTrigger></Triggers>"
        L"<Principals><Principal id=\"User\"><UserId>"+user+L"</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>"
        L"<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"
        L"<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><StartWhenAvailable>false</StartWhenAvailable><RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>"
        L"<IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings><AllowStartOnDemand>true</AllowStartOnDemand>"
        L"<Enabled>true</Enabled><Hidden>true</Hidden><RunOnlyIfIdle>false</RunOnlyIfIdle><WakeToRun>false</WakeToRun><ExecutionTimeLimit>PT30S</ExecutionTimeLimit><Priority>7</Priority><Volatile>true</Volatile></Settings>"
        L"<Actions Context=\"User\"><Exec><Command>"+xmlEscape(exe.wstring())+L"</Command><Arguments>--recover-host</Arguments><WorkingDirectory>"+
        xmlEscape(exe.parent_path().wstring())+L"</WorkingDirectory></Exec></Actions></Task>";
    VARIANT empty{},identity{};Bstr sid(current.sid);identity.vt=VT_BSTR;identity.bstrVal=sid;
    ComPtr<IRegisteredTask> installed;
    checkTask(current.folder->RegisterTask(Bstr(current.name+L".Recovery"),Bstr(xml),TASK_CREATE_OR_UPDATE,identity,empty,TASK_LOGON_INTERACTIVE_TOKEN,empty,&installed),"Arm host recovery task");
    ComPtr<ITaskDefinition> def;ComPtr<ITaskSettings> settings;ComPtr<ITaskSettings3> settings3;VARIANT_BOOL flag=VARIANT_FALSE;
    checkTask(installed->get_Definition(&def),"Verify recovery definition");checkTask(def->get_Settings(&settings),"Verify recovery settings");
    checkTask(settings.As(&settings3),"Verify volatile task support");checkTask(settings3->get_Volatile(&flag),"Verify recovery boot policy");
    if(!flag)throw std::runtime_error("Recovery task would survive reboot with autostart disabled");
}
void checkTaskAction(Task& current) {
    ComPtr<ITaskDefinition> def;ComPtr<IActionCollection> actions;ComPtr<IAction> action;ComPtr<IExecAction> exec;LONG count=0;Bstr path,args;
    checkTask(current.task->get_Definition(&def),"Read launch definition");checkTask(def->get_Actions(&actions),"Read launch actions");
    checkTask(actions->get_Count(&count),"Count launch actions");if(count!=1)throw std::runtime_error("Host task action identity conflict");
    checkTask(actions->get_Item(1,&action),"Read host action");checkTask(action.As(&exec),"Read executable action");
    checkTask(exec->get_Path(&path.value),"Read host launch path");checkTask(exec->get_Arguments(&args.value),"Read host launch arguments");
    if(!path.value || !args.value || _wcsicmp(std::filesystem::path(path.value).lexically_normal().make_preferred().c_str(),tagHostPath().c_str()) || wcscmp(args.value,L"--scheduled"))throw std::runtime_error("Host task executable identity conflict");
}
void startTask(Task& current) {
    checkTaskAction(current);
    DWORD session=0;if(!ProcessIdToSessionId(GetCurrentProcessId(),&session) || !session)throw std::runtime_error("Interactive Windows session required");
    VARIANT empty{};ComPtr<IRunningTask> running;
    checkTask(current.task->RunEx(empty,TASK_RUN_USE_SESSION_ID,session,nullptr,&running),"Start host task");
}
void validate(IRegisteredTask* task,bool login) {
    ComPtr<ITaskDefinition> def;ComPtr<ITaskSettings> settings;ComPtr<IPrincipal> principal;ComPtr<ITriggerCollection> triggers;
    checkTask(task->get_Definition(&def),"Verify task");checkTask(def->get_Settings(&settings),"Verify task settings");
    checkTask(def->get_Principal(&principal),"Verify task principal");checkTask(def->get_Triggers(&triggers),"Verify task triggers");
    Bstr limit,interval;int count=0;LONG triggersCount=0;TASK_LOGON_TYPE logon;TASK_RUNLEVEL_TYPE level;TASK_INSTANCES_POLICY policy;
    checkTask(settings->get_ExecutionTimeLimit(&limit.value),"Verify execution limit");checkTask(settings->get_RestartInterval(&interval.value),"Verify restart interval");
    checkTask(settings->get_RestartCount(&count),"Verify restart count");checkTask(settings->get_MultipleInstances(&policy),"Verify singleton policy");
    checkTask(principal->get_LogonType(&logon),"Verify login type");checkTask(principal->get_RunLevel(&level),"Verify elevation");checkTask(triggers->get_Count(&triggersCount),"Verify trigger count");
    if(!limit.value || wcscmp(limit.value,L"PT0S") || !interval.value || wcscmp(interval.value,L"PT1M") || count!=3 || policy!=TASK_INSTANCES_IGNORE_NEW ||
        logon!=TASK_LOGON_INTERACTIVE_TOKEN || level!=TASK_RUNLEVEL_LUA || triggersCount!=(login?1:0))throw std::runtime_error("Host task verification failed");
    VARIANT_BOOL flag=VARIANT_TRUE;
    checkTask(settings->get_DisallowStartIfOnBatteries(&flag),"Verify battery start");if(flag)throw std::runtime_error("Host task blocked on battery");
    checkTask(settings->get_StopIfGoingOnBatteries(&flag),"Verify battery stop");if(flag)throw std::runtime_error("Host task stops on battery");
    checkTask(settings->get_RunOnlyIfIdle(&flag),"Verify idle dependency");if(flag)throw std::runtime_error("Host task requires idle");
    checkTask(settings->get_RunOnlyIfNetworkAvailable(&flag),"Verify network dependency");if(flag)throw std::runtime_error("Host task requires network");
    checkTask(settings->get_AllowDemandStart(&flag),"Verify demand start");if(!flag)throw std::runtime_error("Host task demand start disabled");
    checkTask(task->get_Enabled(&flag),"Verify task enabled");if(!flag)throw std::runtime_error("Host task disabled");
}
}
std::string tagTaskWarning(){
    std::lock_guard lock(warningMutex);if(!warningText.empty())return warningText;
    RecoveryStore state(0);
    if(state.ours() && state.record.budget.active && state.record.budget.used>=3)
        return "Использованы три попытки восстановления хоста. Следующий сбой потребует «Обновить устройства».";
    return {};
}
std::wstring tagHostMode(const wchar_t* arguments) {
    const std::wstring command=L"mic_tag_host.exe "+std::wstring(arguments?arguments:L"");int count=0;
    auto args=CommandLineToArgvW(command.c_str(),&count);
    if(!args)throw std::runtime_error("Cannot parse host arguments");
    struct Free {LPWSTR* p;~Free(){LocalFree(p);}} free{args};
    if(count>2)throw std::runtime_error("Host accepts one command argument");
    return count==2?args[1]:L"";
}
std::filesystem::path tagHostPath() {
    if(const auto configured=environment(L"MNR_TAG_HOST_PATH");!configured.empty())return std::filesystem::path(configured).lexically_normal().make_preferred();
    wchar_t module[32768]{};const auto length=GetModuleFileNameW(nullptr,module,std::size(module));
    if(!length || length==std::size(module))throw std::runtime_error("Cannot resolve bundled host path");
    const auto bundled=std::filesystem::path(module).parent_path()/L"mic_tag_host.exe";
    if(std::filesystem::is_regular_file(bundled))return bundled;
    // Migration from runtime-core v2. Never prefer it over a bundled (possibly incompatible) host.
    return legacyHostPath();
}
bool tagMaintenancePending(){return std::filesystem::exists(projectRoot()/L".update/hold");}
bool tagTaskEnabled(int mode) {
    if(mode< -1 || mode>1)throw std::runtime_error("Invalid task enable mode");
    // First maintenance creates the demand-start task. Absence is not a saved
    // user choice to disable the task once that operation creates it.
    Task current;if(!current.task)return mode<0;
    if(mode>=0)checkTask(current.task->put_Enabled(mode?VARIANT_TRUE:VARIANT_FALSE),"Change host task enabled state");
    VARIANT_BOOL enabled=VARIANT_FALSE;checkTask(current.task->get_Enabled(&enabled),"Read host task enabled state");
    return enabled!=VARIANT_FALSE;
}
void stopTagHost() {
    TagEndpointStatus status;
    if(!readTagEndpointStatus(status) || !tagEndpointHostAlive(status)) {
        HANDLE initializing=nullptr;
        {RecoveryStore state;initializing=recoveryOwner(state,false,0);}
        if(initializing)CloseHandle(initializing);
        else {
        HANDLE owner=OpenMutexW(SYNCHRONIZE,FALSE,L"Local\\MicNoize.TagHost");
        if(owner){CloseHandle(owner);throw std::runtime_error("Cannot identify the running host for maintenance");}
        HANDLE global=OpenMutexW(SYNCHRONIZE,FALSE,L"Global\\MicNoize.TAG.Driver");
        if(global){CloseHandle(global);throw std::runtime_error("Another Windows session owns the TAG driver; maintenance was cancelled");}
        if(GetLastError()==ERROR_ACCESS_DENIED)throw std::runtime_error("Cannot verify cross-session TAG ownership; maintenance was cancelled");
        }
    } else {
        HANDLE process=OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION|SYNCHRONIZE,FALSE,status.pid);
        if(!process)throw std::runtime_error("Cannot verify host for maintenance");
        struct Close{HANDLE h;~Close(){CloseHandle(h);}} close{process};
        wchar_t path[32768]{};DWORD size=std::size(path),session=0,ours=0;FILETIME started{},exit{},kernel{},user{};
        const auto legacy=legacyHostPath(),current=tagHostPath();
        if(!QueryFullProcessImageNameW(process,0,path,&size) || (_wcsicmp(path,current.c_str()) && _wcsicmp(path,legacy.c_str())) ||
            !GetProcessTimes(process,&started,&exit,&kernel,&user) || CompareFileTime(&started,&status.processStarted) ||
            !ProcessIdToSessionId(status.pid,&session) || !ProcessIdToSessionId(GetCurrentProcessId(),&ours) || session!=ours)
            throw std::runtime_error("Host identity conflict; maintenance did not stop it");
    }
    cancelTagRecovery();
    HANDLE stop=OpenEventW(EVENT_MODIFY_STATE,FALSE,L"Local\\MicNoize.TagHost.Stop.v2");
    if(stop){const auto ok=SetEvent(stop);CloseHandle(stop);if(!ok)throw std::runtime_error("Cannot signal host stop");}
    // The supervisor first grants its verified worker five seconds to stop.
    for(unsigned i=0;i<120;++i) {
        bool alive=false;
        for(const auto name:{L"Local\\MicNoize.TagHost",L"Local\\MicNoize.TagSupervisor.v2"}) {
            HANDLE owner=OpenMutexW(SYNCHRONIZE,FALSE,name);if(owner){alive=true;CloseHandle(owner);}
        }
        if(!alive)return;
        Sleep(100);
    }
    throw std::runtime_error("Host did not stop for maintenance; files were not replaced");
}
void legacyTagHost(int operation) {
    if(operation<0 || operation>3)throw std::runtime_error("Invalid legacy host operation");
    const bool stop=operation==1;
    if(operation!=2 && !tagMaintenancePending())throw std::runtime_error("Legacy host checks require an active maintenance transaction");
    Task current;if(current.task)checkTaskAction(current);
    struct Close {HANDLE h;~Close(){if(h && h!=INVALID_HANDLE_VALUE)CloseHandle(h);}};
    Close managed{OpenMutexW(SYNCHRONIZE,FALSE,L"Global\\MicNoize.TAG.Driver")};
    if(managed.h || GetLastError()==ERROR_ACCESS_DENIED)throw std::runtime_error("A managed TAG host is active; legacy migration was cancelled");
    if(operation==3) {
        using namespace ThinAudioGateway;
        struct Driver {
            HMODULE module=nullptr;TagDriver* api=nullptr;
            ~Driver(){if(api)api->Delete();if(module)FreeLibrary(module);}
        } driver;
        driver.module=LoadLibraryExW((projectRoot()/L"vendor/tag-2.0.0.1903-demo/apidll/x64/tagapi.dll").c_str(),nullptr,LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR|LOAD_LIBRARY_SEARCH_SYSTEM32);
        if(!driver.module)throw std::runtime_error("Cannot inspect TAG lines before legacy rollback");
        using Create=HRESULT (__stdcall*)(void**,const GUID*);
        auto create=reinterpret_cast<Create>(GetProcAddress(driver.module,"Driver_Create"));
        if(!create)throw std::runtime_error("TAG Driver_Create export missing");
        const GUID product={0x4d699d4a,0x65a5,0x40ec,{0x98,0x75,0x8e,0x6d,0x5f,0xc0,0x1e,0x0c}};
        checkTask(create(reinterpret_cast<void**>(&driver.api),&product),"Create legacy rollback inspector");
        checkTask(driver.api->Find(),"Find TAG for legacy rollback");checkTask(driver.api->Open(),"Open TAG for legacy rollback");
        DriverInfo info{};checkTask(driver.api->GetInfo(info),"Inspect rollback driver");
        if(info.VerMajor!=2 || info.NumLines>128)throw std::runtime_error("Invalid rollback driver information");
        std::vector<VirtualLineDesc> lines(std::max(1u,info.NumLines));unsigned count=0;
        checkTask(driver.api->GetLineList(lines.data(),static_cast<unsigned>(lines.size()*sizeof(VirtualLineDesc)),count),"Inspect rollback lines");
        if(count>lines.size() || !tagLegacyRestartSafe(std::span(lines.data(),count)))
            throw std::runtime_error("Legacy rollback needs user action: the old host would delete or adopt another TAG device; it was not started");
        return;
    }
    Close snapshot{CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS,0)},process{nullptr};
    if(snapshot.h==INVALID_HANDLE_VALUE)throw std::runtime_error("Cannot enumerate the legacy host");
    const auto expected=legacyHostPath();
    DWORD ours=0;if(!ProcessIdToSessionId(GetCurrentProcessId(),&ours) || !ours)throw std::runtime_error("Legacy migration requires an interactive logon");
    PROCESSENTRY32W entry{};entry.dwSize=sizeof(entry);
    if(!Process32FirstW(snapshot.h,&entry))throw std::runtime_error("Cannot read the legacy host process list");
    do {
        if(_wcsicmp(entry.szExeFile,L"mic_tag_host.exe"))continue;
        Close candidate{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION|SYNCHRONIZE|(stop?PROCESS_TERMINATE:0),FALSE,entry.th32ProcessID)};
        if(!candidate.h) {
            if(GetLastError()==ERROR_INVALID_PARAMETER)continue;
            throw std::runtime_error("Cannot verify a TAG host; no legacy process was stopped");
        }
        wchar_t path[32768]{};DWORD size=std::size(path),session=0;
        if(!QueryFullProcessImageNameW(candidate.h,0,path,&size))throw std::runtime_error("Legacy executable path unavailable");
        if(_wcsicmp(path,expected.c_str()))continue;
        if(!ProcessIdToSessionId(entry.th32ProcessID,&session) || session!=ours)throw std::runtime_error("Another Windows session owns the legacy host");
        if(process.h)throw std::runtime_error("Multiple legacy hosts; migration was cancelled");
        // Retaining the process handle prevents a reused PID from changing the target.
        process.h=candidate.h;candidate.h=nullptr;
    }while(Process32NextW(snapshot.h,&entry));
    if(GetLastError()!=ERROR_NO_MORE_FILES)throw std::runtime_error("Legacy process enumeration was interrupted");
    if(!process.h) {
        Close owner{OpenMutexW(SYNCHRONIZE,FALSE,L"Local\\MicNoize.TagHost")};
        if(owner.h || GetLastError()==ERROR_ACCESS_DENIED)throw std::runtime_error("Unidentified TAG owner; migration was cancelled");
        if(stop || operation==2)return;
        throw std::runtime_error("Legacy host has not started");
    }
    if(operation==2)return;
    if(stop) {
        // The caller keeps the verified legacy image open without write/delete sharing.
        if(!TerminateProcess(process.h,0) || WaitForSingleObject(process.h,5000)!=WAIT_OBJECT_0)throw std::runtime_error("Verified legacy host did not stop");
        return;
    }
    // Frozen v1 layout, used only to acknowledge Stop after rollback with the UI closed.
    // No new v1 opcode or audio stream is introduced by this compatibility check.
    struct Packet {unsigned version,command,frames,running,capacity,gaps;int result;float samples[16384];};
    static_assert(sizeof(Packet)==65564 && offsetof(Packet,samples)==28);
    Close mapping{OpenFileMappingW(FILE_MAP_ALL_ACCESS,FALSE,L"Local\\MicNoize.TagLink.v1")};
    struct View {Packet* p;~View(){if(p)UnmapViewOfFile(p);}} view{mapping.h?static_cast<Packet*>(MapViewOfFile(mapping.h,FILE_MAP_ALL_ACCESS,0,0,sizeof(Packet))):nullptr};
    Close mutex{OpenMutexW(SYNCHRONIZE|MUTEX_MODIFY_STATE,FALSE,L"Local\\MicNoize.TagLink.Lock")};
    Close request{OpenEventW(EVENT_MODIFY_STATE,FALSE,L"Local\\MicNoize.TagLink.Request")};
    Close response{OpenEventW(SYNCHRONIZE|EVENT_MODIFY_STATE,FALSE,L"Local\\MicNoize.TagLink.Response")};
    if(!view.p || !mutex.h || !request.h || !response.h)throw std::runtime_error("Legacy IPC unavailable");
    const auto locked=[&] {
        const auto result=WaitForSingleObject(mutex.h,500);
        if(result!=WAIT_OBJECT_0 && result!=WAIT_ABANDONED)throw std::runtime_error("Legacy IPC lock timeout");
    };
    locked();
    if(view.p->version!=1){ReleaseMutex(mutex.h);throw std::runtime_error("Legacy protocol mismatch");}
    ResetEvent(response.h);view.p->frames=0;view.p->result=-1;view.p->command=3;ReleaseMutex(mutex.h);
    if(!SetEvent(request.h) || WaitForSingleObject(response.h,500)!=WAIT_OBJECT_0)throw std::runtime_error("Legacy host did not acknowledge Stop");
    locked();const bool acknowledged=view.p->version==1 && view.p->command==0 && view.p->result==0;ReleaseMutex(mutex.h);
    if(!acknowledged || WaitForSingleObject(process.h,0)!=WAIT_TIMEOUT)throw std::runtime_error("Legacy host response invalid");
    unsigned endpoints=0;for(const auto& device:devices(true))if(tagDriverEndpoint(device.id,L"MicNoize Microphone.Capture.Topology"))++endpoints;
    if(endpoints!=1)throw std::runtime_error("Legacy TAG capture endpoint missing or ambiguous");
}
bool tagHostFileCompatible(const std::filesystem::path& path) noexcept {
    const auto file=LoadLibraryExW(path.c_str(),nullptr,LOAD_LIBRARY_AS_DATAFILE|LOAD_LIBRARY_AS_IMAGE_RESOURCE);
    if(!file)return false;
    const auto resource=FindResourceW(file,MAKEINTRESOURCEW(101),RT_RCDATA);
    const auto loaded=resource?LoadResource(file,resource):nullptr;
    const auto bytes=loaded?LockResource(loaded):nullptr;
    unsigned version[2]{};
    if(bytes && SizeofResource(file,resource)==sizeof(version))memcpy(version,bytes,sizeof(version));
    FreeLibrary(file);return version[0]==tagProtocolVersion && version[1]==tagHostBuild;
}
void setTagTaskWarning(const std::string& warning){std::lock_guard lock(warningMutex);warningText=warning;}
bool tagTaskAutostart(){try {Task task;return task.login();}catch(...){return legacyLogin();}}
bool configureTagTask(int login) {
    if(tagMaintenancePending())throw std::runtime_error("Mic Noize device maintenance in progress");
    if(login< -1 || login>1)throw std::runtime_error("Invalid host login mode");
    Task current;const bool enabled=login<0?current.login():login!=0;
    const auto exe=tagHostPath();
    if(!tagHostFileCompatible(exe))throw std::runtime_error("Обновите фоновый компонент Mic Noize: версия хоста отсутствует или несовместима");
    const auto user=xmlEscape(current.sid);
    const std::wstring xml=L"<?xml version=\"1.0\" encoding=\"UTF-16\"?><Task version=\"1.3\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">"
        L"<RegistrationInfo><Author>"+std::wstring(author)+L"</Author><Description>Persistent Mic Noize virtual microphone</Description></RegistrationInfo>"
        L"<Triggers>"+(enabled?L"<LogonTrigger><Enabled>true</Enabled><UserId>"+user+L"</UserId></LogonTrigger>":L"")+L"</Triggers>"
        L"<Principals><Principal id=\"User\"><UserId>"+user+L"</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>"
        L"<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"
        L"<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowHardTerminate>true</AllowHardTerminate><StartWhenAvailable>true</StartWhenAvailable>"
        L"<RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable><IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>"
        L"<AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><Hidden>false</Hidden><RunOnlyIfIdle>false</RunOnlyIfIdle><WakeToRun>false</WakeToRun>"
        L"<ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Priority>6</Priority><RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure></Settings>"
        L"<Actions Context=\"User\"><Exec><Command>"+xmlEscape(exe.wstring())+L"</Command><Arguments>--scheduled</Arguments><WorkingDirectory>"+
        xmlEscape(exe.parent_path().wstring())+L"</WorkingDirectory></Exec></Actions></Task>";
    VARIANT empty{},identity{};Bstr sid(current.sid);identity.vt=VT_BSTR;identity.bstrVal=sid;
    ComPtr<IRegisteredTask> installed;
    checkTask(current.folder->RegisterTask(Bstr(current.name),Bstr(xml),TASK_CREATE_OR_UPDATE,identity,empty,TASK_LOGON_INTERACTIVE_TOKEN,empty,&installed),"Register host task");
    validate(installed.Get(),enabled);
    // Preserve the old login path until the replacement is registered and read back.
    removeLegacy();return enabled;
}
HANDLE tagHostProcess(DWORD access,bool managedOnly) {
    TagEndpointStatus status;
    if(!readTagEndpointStatus(status) || !tagEndpointHostAlive(status)) {
        RecoveryStore state;return recoveryOwner(state,false,access);
    }
    if(status.hostBuild!=tagHostBuild || status.audioVersion!=tagProtocolVersion)throw std::runtime_error("Running host is incompatible with scheduled handoff");
    if(managedOnly) {
        HANDLE managed=OpenMutexW(SYNCHRONIZE,FALSE,L"Local\\MicNoize.TagHost.Scheduled.v2");
        if(!managed)return nullptr;
        CloseHandle(managed);
    }
    return openHostIdentity(status.pid,status.processStarted,access);
}
void runTagTask() {
    if(tagMaintenancePending())throw std::runtime_error("Mic Noize device maintenance in progress");
    Task current;if(!current.task)throw std::runtime_error("Host task missing");
    struct Close {HANDLE h;~Close(){if(h)CloseHandle(h);}} process{tagHostProcess(0)};
    bool retainWorker=false;
    if(process.h) {
        Close managed{OpenMutexW(SYNCHRONIZE,FALSE,L"Local\\MicNoize.TagHost.Scheduled.v2")};
        if(managed.h) {
            retainWorker=true;
            Close supervisor{OpenMutexW(SYNCHRONIZE,FALSE,L"Local\\MicNoize.TagSupervisor.v2")};
            if(supervisor.h){
                RecoveryStore state;
                if(state.ours() && state.record.budget.active && state.record.budget.used>=3){armRecovery(current);state.record.budget.used=0;state.save();}
                return;
            }
            // A surviving worker asks the Scheduler to restore its supervisor, without audio restart.
        } else {
            Close stop{OpenEventW(EVENT_MODIFY_STATE,FALSE,L"Local\\MicNoize.TagHost.Stop.v2")};
            if(!stop.h)throw std::runtime_error("Обновите хост Mic Noize для согласованного перехода в Планировщик");
            if(!SetEvent(stop.h) || WaitForSingleObject(process.h,5000)!=WAIT_OBJECT_0)throw std::runtime_error("Host did not finish scheduled handoff; no foreign process was terminated");
        }
    }
    // A second logon must not replace the first session's recovery intention.
    Close global{OpenMutexW(SYNCHRONIZE,FALSE,L"Global\\MicNoize.TAG.Driver")};const auto globalError=GetLastError();
    Close local{OpenMutexW(SYNCHRONIZE,FALSE,L"Local\\MicNoize.TagHost")};
    if((global.h && !local.h) || (!global.h && globalError==ERROR_ACCESS_DENIED))throw std::runtime_error("Another Windows session owns the TAG driver");
    if(local.h && !process.h)throw std::runtime_error("TAG host initializing; wait for its identity before scheduled handoff");
    armRecovery(current);
    RecoveryStore state;
    if(retainWorker && state.ours() && state.record.budget.active){state.record.budget.used=0;state.record.budget.missingAt=0;state.save();}
    else state.begin();
    startTask(current);
}
bool publishTagOwner(const GUID& generation,bool supervisor) {
    RecoveryStore state;if(!state.ours() || !state.record.budget.current(generation))return false;
    RecoveryOwner owner;owner.generation=generation;owner.pid=GetCurrentProcessId();FILETIME ended{},kernel{},user{};
    if(!GetProcessTimes(GetCurrentProcess(),&owner.started,&ended,&kernel,&user))throw std::runtime_error("Host creation time unavailable");
    checkTask(HRESULT_FROM_WIN32(RegSetValueExW(state.key,supervisor?L"Supervisor":L"Worker",0,REG_BINARY,reinterpret_cast<const BYTE*>(&owner),sizeof(owner))),"Publish host owner");return true;
}
GUID scheduledTagRecovery() {
    Task current;if(!current.task)throw std::runtime_error("Host task missing");
    RecoveryStore state;
    if(!state.ours()) { // Logon trigger starts a new session; delayed same-logon commands never do.
        HANDLE global=OpenMutexW(SYNCHRONIZE,FALSE,L"Global\\MicNoize.TAG.Driver");
        if(global){CloseHandle(global);return GUID_NULL;}
        if(GetLastError()==ERROR_ACCESS_DENIED)return GUID_NULL;
        armRecovery(current);state.begin();
    }
    if(!state.record.budget.active)return GUID_NULL;
    state.record.budget.missingAt=0;state.save();return state.record.budget.generation;
}
bool tagRecoveryCurrent(const GUID& generation) {
    RecoveryStore state;return state.ours() && state.record.budget.current(generation);
}
bool takeTagRecovery(const GUID& generation) {
    RecoveryStore state;
    if(!state.ours() || !state.record.budget.take(generation))return false;
    state.save();return true;
}
void tagRecoveryPhase(TagDeviceState phase,const GUID& generation) {
    RecoveryStore state;if(!state.ours() || !state.record.budget.current(generation))return;
    const RecoveryPhase value{generation,phase};
    checkTask(HRESULT_FROM_WIN32(RegSetValueExW(state.key,L"Phase",0,REG_BINARY,reinterpret_cast<const BYTE*>(&value),sizeof(value))),"Publish recovery phase");
}
TagDeviceState tagDeviceState(std::string& detail) {
    TagEndpointStatus endpoint;
    if(readTagEndpointStatus(endpoint) && tagEndpointHostAlive(endpoint)) {
        if(endpoint.hostBuild!=tagHostBuild || endpoint.audioVersion!=tagProtocolVersion){detail="Обновите UI и фоновый компонент вместе";return TagDeviceState::UserAction;}
        if(endpoint.ready)return TagDeviceState::Ready;
        detail=endpoint.error;
        if(detail.empty() || tagTransientStartup(detail) || detail.find("Waiting for")!=detail.npos || detail.find("not active")!=detail.npos ||
           detail.find("0x88890010")!=detail.npos || detail.find("0x88890004")!=detail.npos)return TagDeviceState::WaitingEndpoint;
        return TagDeviceState::UserAction;
    }
    RecoveryStore state;
    if(!state.ours() || !state.record.budget.active){detail="Нажмите «Обновить устройства» для проверки хоста";return TagDeviceState::UserAction;}
    HANDLE supervisor=OpenMutexW(SYNCHRONIZE,FALSE,L"Local\\MicNoize.TagSupervisor.v2");
    if(!supervisor) {
        if(GetLastError()==ERROR_ACCESS_DENIED){detail="Нет доступа к состоянию фонового хоста";return TagDeviceState::UserAction;}
        if(state.record.budget.used>=3){detail="Исчерпаны три попытки восстановления; нажмите «Обновить устройства»";return TagDeviceState::UserAction;}
        return TagDeviceState::Recovering;
    }
    CloseHandle(supervisor);
    RecoveryPhase phase;DWORD size=sizeof(phase),type=0;
    const auto result=RegQueryValueExW(state.key,L"Phase",nullptr,&type,reinterpret_cast<BYTE*>(&phase),&size);
    if(result==ERROR_FILE_NOT_FOUND)return TagDeviceState::Starting;
    checkTask(HRESULT_FROM_WIN32(result),"Read recovery phase");
    if(type!=REG_BINARY || size!=sizeof(phase) || static_cast<unsigned>(phase.state)>5 || phase.state==TagDeviceState::Ready || phase.state==TagDeviceState::WaitingEndpoint)throw std::runtime_error("Invalid recovery phase");
    return phase.generation==state.record.budget.generation?phase.state:TagDeviceState::Starting;
}
void cancelTagRecovery(const GUID& generation) {
    {
        RecoveryStore state;
        if(!state.ours() || (generation!=GUID_NULL && state.record.budget.generation!=generation))return;
        state.record.budget.active=0;state.save();
    }
    // Persist cancellation even when the Scheduler service is unavailable. A queued
    // recovery reads that cancellation before starting anything.
    try {Task current;if(const auto recovery=current.recovery())checkTask(recovery->put_Enabled(VARIANT_FALSE),"Disable host recovery task");}
    catch(const std::exception& error){setTagTaskWarning(std::string("Фоновый запуск отменён; проверка Планировщика недоступна: ")+error.what());}
}
bool recoverTagTask() {
    if(tagMaintenancePending())return false;
    Task current;if(!current.task)return false;
    VARIANT_BOOL enabled=VARIANT_FALSE;checkTask(current.task->get_Enabled(&enabled),"Read recovery permission");if(!enabled)return false;
    RecoveryStore state;if(!state.ours() || !state.record.budget.active)return false;
    struct Close {HANDLE h;~Close(){if(h)CloseHandle(h);}} supervisor{OpenMutexW(SYNCHRONIZE,FALSE,L"Local\\MicNoize.TagSupervisor.v2")};
    if(!supervisor.h && GetLastError()==ERROR_ACCESS_DENIED)throw std::runtime_error("Cannot verify supervisor ownership");
    auto& budget=state.record.budget;const auto now=tagAwakeMilliseconds();
    if(supervisor.h) {
        Close pulse{OpenEventW(SYNCHRONIZE,FALSE,L"Local\\MicNoize.TagSupervisor.Heartbeat.v1")};
        if(pulse.h && WaitForSingleObject(pulse.h,0)==WAIT_OBJECT_0) {
            if(budget.missingAt){budget.missingAt=0;state.save();}
            return true;
        }
    }
    if(budget.used>=3){if(const auto recovery=current.recovery())checkTask(recovery->put_Enabled(VARIANT_FALSE),"Suspend exhausted recovery task");return false;}
    if(!budget.missingAt){budget.missingAt=now;state.save();return true;}
    if(now<budget.missingAt || now-budget.missingAt<60000)return true;
    Close stalled{supervisor.h?recoveryOwner(state,true,PROCESS_TERMINATE):nullptr};
    if(supervisor.h && !stalled.h)throw std::runtime_error("Stalled supervisor identity unavailable; no process was stopped");
    if(!budget.take(budget.generation))return false;
    budget.missingAt=now;state.save(); // Persist the claim before launching; crashes cannot replenish it.
    if(stalled.h) {
        if(!TerminateProcess(stalled.h,1) || WaitForSingleObject(stalled.h,5000)!=WAIT_OBJECT_0)throw std::runtime_error("Verified stalled supervisor did not stop");
        // IgnoreNew would discard RunEx while Scheduler still reports the dead action.
        TASK_STATE running=TASK_STATE_UNKNOWN;
        for(unsigned i=0;i<50;++i) {
            checkTask(current.task->get_State(&running),"Read stopped supervisor task");
            if(running!=TASK_STATE_RUNNING)break;
            Sleep(100);
        }
        if(running==TASK_STATE_RUNNING)throw std::runtime_error("Scheduler has not released the stopped supervisor");
        CloseHandle(supervisor.h);supervisor.h=nullptr;
    }
    startTask(current);
    return true;
}
void removeTagTask() {
    Task current;
    if(current.task)checkTaskAction(current);
    cancelTagRecovery();
    if(current.recovery())checkTask(current.folder->DeleteTask(Bstr(current.name+L".Recovery"),0),"Remove host recovery task");
    if(current.task)checkTask(current.folder->DeleteTask(Bstr(current.name),0),"Remove host task");
    removeLegacy();
}
}
