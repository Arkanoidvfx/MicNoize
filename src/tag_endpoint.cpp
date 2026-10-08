#include "tag_endpoint.hpp"
#include "audio.hpp"
#include <devicetopology.h>
#include <functiondiscoverykeys_devpkey.h>
#include <setupapi.h>
#include <wrl/client.h>
#include <fstream>
#include <thread>

namespace mic {
namespace {
using Microsoft::WRL::ComPtr;
constexpr wchar_t statusName[]=L"Local\\MicNoize.TagEndpoint.v2";
constexpr wchar_t lockName[]=L"Local\\MicNoize.TagEndpoint.Lock.v2";
constexpr GUID volumeContext={0x067e7387,0x7605,0x4377,{0x8a,0xbc,0x2d,0x1b,0x1b,0xdb,0x70,0xa4}};
struct Handle {HANDLE h=nullptr;~Handle(){if(h)CloseHandle(h);}};
void checked(HRESULT hr,const char* action) {
    if(FAILED(hr)){char s[200];sprintf_s(s,"%s: HRESULT 0x%08lX",action,static_cast<unsigned long>(hr));throw std::runtime_error(s);}
}
std::wstring property(IMMDevice* device,REFPROPERTYKEY key) {
    ComPtr<IPropertyStore> store;checked(device->OpenPropertyStore(STGM_READ,&store),"TAG properties");
    PROPVARIANT value{};checked(store->GetValue(key,&value),"TAG property");
    std::wstring result=value.vt==VT_LPWSTR && value.pwszVal?value.pwszVal:L"";
    PropVariantClear(&value);return result;
}
std::wstring driverInstance(const std::wstring& path) {
    const auto set=SetupDiCreateDeviceInfoList(nullptr,nullptr);
    if(set==INVALID_HANDLE_VALUE)throw std::runtime_error("TAG device information unavailable");
    struct Cleanup {HDEVINFO set;~Cleanup(){SetupDiDestroyDeviceInfoList(set);}} cleanup{set};
    SP_DEVICE_INTERFACE_DATA interfaceData{};interfaceData.cbSize=sizeof(interfaceData);
    if(!SetupDiOpenDeviceInterfaceW(set,path.c_str(),0,&interfaceData))checked(HRESULT_FROM_WIN32(GetLastError()),"TAG driver interface");
    SP_DEVINFO_DATA info{};info.cbSize=sizeof(info);DWORD size=0;
    SetupDiGetDeviceInterfaceDetailW(set,&interfaceData,nullptr,0,&size,nullptr);
    if(size<sizeof(SP_DEVICE_INTERFACE_DETAIL_DATA_W) || size>65536)throw std::runtime_error("TAG interface detail size");
    std::vector<BYTE> buffer(size);auto detail=reinterpret_cast<SP_DEVICE_INTERFACE_DETAIL_DATA_W*>(buffer.data());detail->cbSize=sizeof(*detail);
    if(!SetupDiGetDeviceInterfaceDetailW(set,&interfaceData,detail,size,nullptr,&info))checked(HRESULT_FROM_WIN32(GetLastError()),"TAG interface detail");
    wchar_t id[512]{};
    if(!SetupDiGetDeviceInstanceIdW(set,&info,id,512,nullptr))checked(HRESULT_FROM_WIN32(GetLastError()),"TAG driver instance");
    return id;
}
ComPtr<IMMDevice> adapterFor(IMMDeviceEnumerator* enumerator,IMMDevice* endpoint) {
    ComPtr<IDeviceTopology> topology;ComPtr<IConnector> connector;ComPtr<IMMDevice> adapter;
    checked(endpoint->Activate(__uuidof(IDeviceTopology),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(topology.GetAddressOf())),"Endpoint topology");
    checked(topology->GetConnector(0,&connector),"Endpoint connector");
    LPWSTR id=nullptr;const auto hr=connector->GetDeviceIdConnectedTo(&id);
    const std::wstring adapterId=SUCCEEDED(hr)&&id?id:L"";CoTaskMemFree(id);
    checked(hr,"Endpoint adapter identity");checked(enumerator->GetDevice(adapterId.c_str(),&adapter),"Endpoint adapter");return adapter;
}
ComPtr<IMMDevice> resolve(IMMDeviceEnumerator* enumerator,const std::wstring& instance,const std::wstring& interfaceName) {
    ComPtr<IMMDeviceCollection> list;checked(enumerator->EnumAudioEndpoints(eCapture,DEVICE_STATE_ACTIVE|DEVICE_STATE_DISABLED,&list),"TAG endpoint list");
    UINT count=0;checked(list->GetCount(&count),"TAG endpoint count");ComPtr<IMMDevice> found;
    bool disabled=false;
    for(UINT i=0;i<count;++i) {
        ComPtr<IMMDevice> endpoint,adapter;
        checked(list->Item(i,&endpoint),"TAG endpoint item");
        try{adapter=adapterFor(enumerator,endpoint.Get());}catch(const std::exception&){continue;}
        if(_wcsicmp(property(adapter.Get(),PKEY_Device_InstanceId).c_str(),instance.c_str()) ||
           property(adapter.Get(),PKEY_DeviceInterface_FriendlyName)!=interfaceName)continue;
        DWORD state=0;checked(endpoint->GetState(&state),"TAG matched endpoint state");
        if(state&DEVICE_STATE_DISABLED){disabled=true;continue;}
        if(state!=DEVICE_STATE_ACTIVE)continue;
        if(found)throw std::runtime_error("Multiple active endpoints match the TAG driver line");
        found=endpoint;
    }
    if(!found && disabled)throw std::runtime_error("TAG microphone endpoint disabled in Windows; enable it in Sound settings");
    if(!found)throw std::runtime_error("Waiting for the TAG microphone endpoint in Windows");
    return found;
}
void logControl(const std::string& message) noexcept {
    try {
        const auto dir=projectRoot()/L"results";std::filesystem::create_directories(dir);
        const auto path=dir/L"tag-endpoint.log";
        if(std::filesystem::exists(path) && std::filesystem::file_size(path)>256*1024) {
            std::error_code ec;std::filesystem::remove(dir/L"tag-endpoint.previous.log",ec);
            std::filesystem::rename(path,dir/L"tag-endpoint.previous.log",ec);
        }
        SYSTEMTIME t{};GetSystemTime(&t);char timestamp[40];
        sprintf_s(timestamp,"%04u-%02u-%02uT%02u:%02u:%02uZ ",t.wYear,t.wMonth,t.wDay,t.wHour,t.wMinute,t.wSecond);
        std::ofstream(path,std::ios::app)<<timestamp<<message<<'\n';
    }catch(...){}
}
// Callbacks only signal the worker; no COM calls, waits, file I/O or allocation.
class Notifications final : public IMMNotificationClient,public IAudioEndpointVolumeCallback {
    std::atomic<ULONG> refs_{1};
public:
    HANDLE event;
    std::atomic<bool> devices{true},externalVolume{false};
    explicit Notifications(HANDLE wake):event(wake){}
    ULONG STDMETHODCALLTYPE AddRef() override{return ++refs_;}
    ULONG STDMETHODCALLTYPE Release() override{const auto n=--refs_;if(!n)delete this;return n;}
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID id,void** out) override {
        if(!out)return E_POINTER;*out=nullptr;
        if(id==__uuidof(IUnknown) || id==__uuidof(IMMNotificationClient))*out=static_cast<IMMNotificationClient*>(this);
        else if(id==__uuidof(IAudioEndpointVolumeCallback))*out=static_cast<IAudioEndpointVolumeCallback*>(this);
        else return E_NOINTERFACE;
        AddRef();return S_OK;
    }
    HRESULT device(){devices=true;SetEvent(event);return S_OK;}
    HRESULT STDMETHODCALLTYPE OnDeviceStateChanged(LPCWSTR,DWORD) override{return device();}
    HRESULT STDMETHODCALLTYPE OnDeviceAdded(LPCWSTR) override{return device();}
    HRESULT STDMETHODCALLTYPE OnDeviceRemoved(LPCWSTR) override{return device();}
    HRESULT STDMETHODCALLTYPE OnDefaultDeviceChanged(EDataFlow,ERole,LPCWSTR) override{return S_OK;}
    HRESULT STDMETHODCALLTYPE OnPropertyValueChanged(LPCWSTR,const PROPERTYKEY) override{return device();}
    HRESULT STDMETHODCALLTYPE OnNotify(PAUDIO_VOLUME_NOTIFICATION_DATA data) override {
        if(!data)return E_POINTER;
        if(data->guidEventContext!=volumeContext){externalVolume=true;SetEvent(event);}
        return S_OK;
    }
};
}
bool tagDriverEndpoint(const std::wstring& endpoint,std::wstring_view topology) {
    const auto init=CoInitializeEx(nullptr,COINIT_MULTITHREADED);
    if(init!=RPC_E_CHANGED_MODE)checked(init,"Endpoint identity COM");
    struct Cleanup {HRESULT init;~Cleanup(){if(SUCCEEDED(init))CoUninitialize();}} cleanup{init};
    ComPtr<IMMDeviceEnumerator> enumerator;ComPtr<IMMDevice> device;
    checked(CoCreateInstance(__uuidof(MMDeviceEnumerator),nullptr,CLSCTX_ALL,IID_PPV_ARGS(&enumerator)),"Endpoint identity enumerator");
    checked(enumerator->GetDevice(endpoint.c_str(),&device),"Endpoint identity lookup");
    const auto adapter=adapterFor(enumerator.Get(),device.Get());
    const auto instance=property(adapter.Get(),PKEY_Device_InstanceId);
    constexpr wchar_t prefix[]=L"ROOT\\THINAUDIOGATEWAY_4D699D4A\\";
    return !_wcsnicmp(instance.c_str(),prefix,std::size(prefix)-1) && (topology.empty() ||
        property(adapter.Get(),PKEY_DeviceInterface_FriendlyName)==topology);
}
bool holdTagEndpointSharedMode(IMMDevice* endpoint) {
    // Windows Sound's "Allow applications to take exclusive control" property.
    // This key is not in the public SDK; reject unexpected types and verify writes.
    constexpr PROPERTYKEY allowExclusive={{0xb3f8fa53,0x0004,0x438e,{0x90,0x03,0x51,0xa4,0x6e,0x13,0x9b,0xfc}},3};
    auto disabled=[&] {
        ComPtr<IPropertyStore> store;checked(endpoint->OpenPropertyStore(STGM_READ,&store),"TAG exclusive policy read");
        PROPVARIANT value{};checked(store->GetValue(allowExclusive,&value),"TAG exclusive policy value");
        const auto type=value.vt;const auto number=value.ulVal;PropVariantClear(&value);
        if(type!=VT_EMPTY && type!=VT_UI4)throw std::runtime_error("Unsupported TAG exclusive policy type");
        return type==VT_UI4 && number==0;
    };
    if(disabled())return false;
    PROPVARIANT value{};value.vt=VT_UI4;value.ulVal=0;
    setEndpointProperty(endpoint,allowExclusive,value);
    if(!disabled())throw std::runtime_error("TAG shared-only policy readback failed");
    return true;
}
bool tagEndpointHostAlive(const TagEndpointStatus& status) {
    if(!status.pid)return false;
    Handle process{OpenProcess(SYNCHRONIZE|PROCESS_QUERY_LIMITED_INFORMATION,FALSE,status.pid)};
    if(!process.h || WaitForSingleObject(process.h,0)!=WAIT_TIMEOUT)return false;
    FILETIME start{},exit{},kernel{},user{};
    return GetProcessTimes(process.h,&start,&exit,&kernel,&user) && CompareFileTime(&start,&status.processStarted)==0;
}
bool readTagEndpointStatus(TagEndpointStatus& result) {
    Handle map{OpenFileMappingW(FILE_MAP_READ,FALSE,statusName)};
    if(!map.h)return false;
    Handle mutex{OpenMutexW(SYNCHRONIZE|MUTEX_MODIFY_STATE,FALSE,lockName)};
    if(!mutex.h)throw std::runtime_error("TAG endpoint status lock unavailable");
    const auto data=static_cast<const TagEndpointStatus*>(MapViewOfFile(map.h,FILE_MAP_READ,0,0,sizeof(TagEndpointStatus)));
    if(!data)throw std::runtime_error("TAG endpoint status mapping unavailable");
    struct Unmap {const void* p;~Unmap(){UnmapViewOfFile(p);}} unmap{data};
    const auto wait=WaitForSingleObject(mutex.h,100);
    if(wait!=WAIT_OBJECT_0 && wait!=WAIT_ABANDONED)throw std::runtime_error("TAG endpoint status timeout");
    result=*data;ReleaseMutex(mutex.h);
    if(result.version!=2 || result.bytes!=sizeof(result) || result.endpoint[511] || result.error[255] || result.ready>1 ||
       !std::isfinite(result.compensation) || result.compensation<=0 ||
       (result.ready && (!result.pid || !result.line || result.hostId==GUID_NULL || !result.endpoint[0])))
        throw std::runtime_error("TAG endpoint status version/size mismatch");
    if(!tagEndpointHostAlive(result) || GetTickCount64()-result.checkedAt>2500)result.ready=0;
    return true;
}
struct TagEndpointGuard::Impl {
    Handle wake{CreateEventW(nullptr,FALSE,FALSE,nullptr)},mapping,mutex;
    TagEndpointStatus* mapped=nullptr;
    TagEndpointStatus status;
    std::atomic<bool> healthy=false;
    std::atomic<ULONGLONG> pulse=0;
    std::atomic<ULONGLONG> awakePulse=tagAwakeMilliseconds();
    std::jthread worker;
    void publish() {
        awakePulse=tagAwakeMilliseconds();
        status.checkedAt=GetTickCount64();pulse=status.checkedAt;
        const auto wait=WaitForSingleObject(mutex.h,100);
        if(wait!=WAIT_OBJECT_0 && wait!=WAIT_ABANDONED)throw std::runtime_error("TAG endpoint publisher lock timeout");
        // If the publisher dies while holding the mutex, an abandoned reader must
        // never see ready=1 paired with a partially copied endpoint/gain snapshot.
        mapped->ready=0;MemoryBarrier();
        auto pending=status;pending.ready=0;*mapped=pending;MemoryBarrier();mapped->ready=status.ready;
        ReleaseMutex(mutex.h);
    }
    Impl(const std::wstring& driverInterface,const std::wstring& ksName,unsigned line,const GUID& host) {
        try {
            if(!wake.h)throw std::runtime_error("TAG control event");
            mapping.h=CreateFileMappingW(INVALID_HANDLE_VALUE,nullptr,PAGE_READWRITE,0,sizeof(status),statusName);
            mutex.h=CreateMutexW(nullptr,FALSE,lockName);
            if(!mapping.h || !mutex.h)throw std::runtime_error("TAG control mapping");
            mapped=static_cast<TagEndpointStatus*>(MapViewOfFile(mapping.h,FILE_MAP_ALL_ACCESS,0,0,sizeof(status)));
            if(!mapped)throw std::runtime_error("TAG control view");
            status.pid=GetCurrentProcessId();status.line=line;status.hostId=host;
            FILETIME exit{},kernel{},user{};
            if(!GetProcessTimes(GetCurrentProcess(),&status.processStarted,&exit,&kernel,&user))throw std::runtime_error("TAG host process identity");
            strcpy_s(status.error,"Waiting for the TAG microphone endpoint in Windows");publish();
            worker=std::jthread([this,driverInterface,ksName](std::stop_token stop){run(stop,driverInterface,ksName);});
        }catch(...){if(mapped)UnmapViewOfFile(mapped);mapped=nullptr;throw;}
    }
    ~Impl(){worker.request_stop();SetEvent(wake.h);if(worker.joinable())worker.join();if(mapped)UnmapViewOfFile(mapped);}
    void run(std::stop_token stop,const std::wstring& driverInterface,const std::wstring& ksName) noexcept {
        bool com=false;ComPtr<IMMDeviceEnumerator> enumerator;ComPtr<IMMDevice> endpoint;ComPtr<IAudioEndpointVolume> level;
        auto* notify=new(std::nothrow) Notifications(wake.h);bool registered=false,volumeRegistered=false;
        std::string previousError;ULONGLONG lastVolumeLog=0;
        auto detach=[&]{if(volumeRegistered){level->UnregisterControlChangeNotify(notify);volumeRegistered=false;}level.Reset();endpoint.Reset();};
        try {
            if(!notify)throw std::runtime_error("TAG callback allocation");
            checked(CoInitializeEx(nullptr,COINIT_MULTITHREADED),"TAG control COM");com=true;
            const auto instance=driverInstance(driverInterface),wanted=ksName+L".Capture.Topology";
            while(!stop.stop_requested()) {
                try {
                    if(!enumerator) {
                        checked(CoCreateInstance(__uuidof(MMDeviceEnumerator),nullptr,CLSCTX_ALL,IID_PPV_ARGS(&enumerator)),"TAG control enumerator");
                        checked(enumerator->RegisterEndpointNotificationCallback(notify),"TAG device notifications");registered=true;
                    }
                    if(notify->devices.exchange(false) && endpoint) {
                        auto current=resolve(enumerator.Get(),instance,wanted);
                        LPWSTR id=nullptr;checked(current->GetId(&id),"TAG current endpoint ID");
                        const bool changed=wcscmp(status.endpoint,id)!=0;CoTaskMemFree(id);
                        if(changed)detach();
                    }
                    if(!endpoint) {
                        healthy=false;status.ready=0;publish();
                        endpoint=resolve(enumerator.Get(),instance,wanted);
                        LPWSTR id=nullptr;checked(endpoint->GetId(&id),"TAG bound endpoint ID");
                        const std::wstring endpointId=id;CoTaskMemFree(id);
                        if(endpointId.size()>=std::size(status.endpoint))throw std::runtime_error("TAG endpoint ID exceeds status capacity");
                        wcscpy_s(status.endpoint,endpointId.c_str());
                        checked(endpoint->Activate(__uuidof(IAudioEndpointVolume),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(level.GetAddressOf())),"TAG bound volume");
                        checked(level->RegisterControlChangeNotify(notify),"TAG volume notifications");volumeRegistered=true;
                    }
                    DWORD state=0;checked(endpoint->GetState(&state),"TAG current endpoint state");
                    if(state!=DEVICE_STATE_ACTIVE)throw std::runtime_error("TAG microphone endpoint is not active");
                    if(holdTagEndpointSharedMode(endpoint.Get()))logControl("Disabled exclusive WASAPI access for bound endpoint "+utf8(status.endpoint));
                    status.compensation=holdTagEndpointLevel(level.Get(),true,&volumeContext);
                    status.ready=1;status.error[0]=0;publish();healthy=true;
                    if(!previousError.empty()){logControl("Recovered endpoint "+utf8(status.endpoint));previousError.clear();}
                    if(notify->externalVolume.exchange(false) && GetTickCount64()-lastVolumeLog>=1000) {
                        logControl("External volume/mute change; restored virtual microphone to 100% and unmuted");lastVolumeLog=GetTickCount64();
                    }
                }catch(const std::exception& error) {
                    healthy=false;status.ready=0;strncpy_s(status.error,error.what(),_TRUNCATE);publish();
                    if(previousError!=error.what()){logControl(error.what());previousError=error.what();}
                    detach();
                    if(registered){enumerator->UnregisterEndpointNotificationCallback(notify);registered=false;}enumerator.Reset();
                }
                WaitForSingleObject(wake.h,1000);
            }
        }catch(const std::exception& error){healthy=false;status.ready=0;strncpy_s(status.error,error.what(),_TRUNCATE);try{publish();}catch(...){}logControl(error.what());}
        healthy=false;status.ready=0;try{publish();}catch(...){}
        detach();if(registered)enumerator->UnregisterEndpointNotificationCallback(notify);
        enumerator.Reset();if(notify)notify->Release();if(com)CoUninitialize();
    }
};
TagEndpointGuard::TagEndpointGuard(const std::wstring& driverInterface,const std::wstring& ksName,unsigned line,const GUID& host):impl_(std::make_unique<Impl>(driverInterface,ksName,line,host)){}
TagEndpointGuard::~TagEndpointGuard()=default;
bool TagEndpointGuard::ready()const{return impl_->healthy.load() && GetTickCount64()-impl_->pulse.load()<2500;}
bool TagEndpointGuard::responsive()const{return tagAwakeMilliseconds()-impl_->awakePulse.load()<15000;}
}
