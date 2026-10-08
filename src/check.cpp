#include "audio.hpp"
#include "tag_link.hpp"
#include "tag.hpp"
#include <cmath>
#include <iostream>
#include <stdexcept>
#include <chrono>
#include <audioclient.h>
#include <mmdeviceapi.h>
#include <wrl/client.h>
#include <fstream>

struct Packet { uint64_t qpc; uint32_t frames, flags; size_t offset; };
struct Received {
    uint64_t frames=0; float peak=0; std::string error;
    std::array<uint64_t,5> phaseFrames{};
    std::array<float,5> phasePeak{};
    std::atomic<bool> ready=false;
    std::vector<float> audio; std::vector<Packet> packets;
};
// Discard samples in smoke tests; retain them only for explicit --record-pair measurements.
static void receive(std::stop_token stop,const std::wstring& id,Received& result,std::atomic<bool>* record=nullptr,bool loopback=false,unsigned rate=mic::rate,WORD channels=2,std::atomic<unsigned>* phase=nullptr) {
    auto check=[](HRESULT hr,const char* where) { if(FAILED(hr)) throw std::runtime_error(where); };
    HRESULT init=CoInitializeEx(nullptr,COINIT_MULTITHREADED);
    if(FAILED(init)) { result.error="Receiver COM initialization"; return; }
    {
        using Microsoft::WRL::ComPtr;
        ComPtr<IMMDeviceEnumerator> e; ComPtr<IMMDevice> d;
        ComPtr<IAudioClient> client; ComPtr<IAudioCaptureClient> capture;
        HANDLE event=CreateEventW(nullptr,FALSE,FALSE,nullptr);
        bool started=false;
        try {
            if(record) { result.audio.reserve(mic::rate*16); result.packets.reserve(2000); }
            if(!event) throw std::runtime_error("Receiver event");
            check(CoCreateInstance(__uuidof(MMDeviceEnumerator),nullptr,CLSCTX_ALL,IID_PPV_ARGS(&e)),"Receiver enumerator");
            check(e->GetDevice(id.c_str(),&d),"Receiver endpoint");
            check(d->Activate(__uuidof(IAudioClient),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(client.GetAddressOf())),"Receiver activation");
            const WORD align=static_cast<WORD>(channels*sizeof(float));
            WAVEFORMATEX format{WAVE_FORMAT_IEEE_FLOAT,channels,rate,rate*align,align,32,0};
            check(client->Initialize(AUDCLNT_SHAREMODE_SHARED,AUDCLNT_STREAMFLAGS_EVENTCALLBACK|AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM|AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY|(loopback?AUDCLNT_STREAMFLAGS_LOOPBACK:0),200000,0,&format,nullptr),"Receiver initialization");
            check(client->SetEventHandle(event),"Receiver set event"); check(client->GetService(IID_PPV_ARGS(&capture)),"Receiver capture service");
            check(client->Start(),"Receiver start"); started=true;
            result.ready=true;
            while(!stop.stop_requested()) {
                WaitForSingleObject(event,100);
                UINT32 n=0; check(capture->GetNextPacketSize(&n),"Receiver packet size");
                while(n) {
                    BYTE* bytes=nullptr; DWORD flags=0; UINT64 qpc=0;
                    check(capture->GetBuffer(&bytes,&n,&flags,nullptr,&qpc),"Receiver buffer");
                    result.frames+=n;
                    const auto stage=phase?std::min(phase->load(),4u):0u;
                    result.phaseFrames[stage]+=n;
                    if(record && record->load() && result.audio.size()+n<=mic::rate*16) {
                        result.packets.push_back({qpc,n,flags,result.audio.size()});
                        const auto samples=reinterpret_cast<const float*>(bytes);
                        for(unsigned i=0;i<n;++i) {
                            float value=0;
                            if(!(flags&AUDCLNT_BUFFERFLAGS_SILENT)) for(unsigned ch=0;ch<channels;++ch) value+=samples[i*channels+ch]/channels;
                            result.audio.push_back(value);
                        }
                    }
                    if(!(flags&AUDCLNT_BUFFERFLAGS_SILENT)) {
                        auto samples=reinterpret_cast<float*>(bytes);
                        for(unsigned i=0;i<n*channels;++i) { if(!std::isfinite(samples[i])) result.error="Non-finite received audio"; else {result.peak=std::max(result.peak,std::abs(samples[i]));result.phasePeak[stage]=std::max(result.phasePeak[stage],std::abs(samples[i]));} }
                    }
                    check(capture->ReleaseBuffer(n),"Receiver release"); check(capture->GetNextPacketSize(&n),"Receiver next packet");
                }
            }
        } catch(const std::exception& ex) { result.error=ex.what(); }
        if(started) client->Stop(); if(event) CloseHandle(event);
    }
    CoUninitialize();
}

static void require(bool v,const char* message) { if(!v) throw std::runtime_error(message); }
static void saveRecording(const std::filesystem::path& path,const Received& r) {
    require(r.error.empty(),r.error.c_str()); require(!r.packets.empty(),"No recorded packets");
    std::ofstream f(path,std::ios::binary); f.exceptions(std::ios::failbit|std::ios::badbit);
    for(const auto& p:r.packets) {
        f.write(reinterpret_cast<const char*>(&p.qpc),8);
        f.write(reinterpret_cast<const char*>(&p.frames),4);
        f.write(reinterpret_cast<const char*>(&p.flags),4);
        f.write(reinterpret_cast<const char*>(r.audio.data()+p.offset),p.frames*4);
    }
}
static void probeCable(const mic::Device& output,const mic::Device& input,const std::filesystem::path& folder) {
    require(output.name==L"CABLE Input (VB-Audio Virtual Cable)" && input.name==L"CABLE Output (VB-Audio Virtual Cable)","Probe requires the VB-CABLE pair");
    require(!std::filesystem::exists(folder),"Use a new probe directory");
    std::filesystem::create_directories(folder);
    auto check=[](HRESULT hr,const char* message){require(SUCCEEDED(hr),message);};
    check(CoInitializeEx(nullptr,COINIT_MULTITHREADED),"Probe COM");
    struct ComEnd { ~ComEnd(){CoUninitialize();} } comEnd;
    using Microsoft::WRL::ComPtr;
    ComPtr<IMMDeviceEnumerator> e; ComPtr<IMMDevice> device;
    ComPtr<IAudioClient> client; ComPtr<IAudioRenderClient> render;
    check(CoCreateInstance(__uuidof(MMDeviceEnumerator),nullptr,CLSCTX_ALL,IID_PPV_ARGS(&e)),"Probe enumerator");
    check(e->GetDevice(output.id.c_str(),&device),"Probe endpoint");
    check(device->Activate(__uuidof(IAudioClient),CLSCTX_ALL,nullptr,reinterpret_cast<void**>(client.GetAddressOf())),"Probe activate");
    WAVEFORMATEX format{WAVE_FORMAT_IEEE_FLOAT,2,mic::rate,mic::rate*8,8,32,0};
    check(client->Initialize(AUDCLNT_SHAREMODE_SHARED,AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM|AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,200000,0,&format,nullptr),"Probe initialize");
    check(client->GetService(IID_PPV_ARGS(&render)),"Probe render");
    UINT32 capacity=0; check(client->GetBufferSize(&capacity),"Probe capacity");
    Received sent,received; sent.audio.reserve(mic::rate*14); sent.packets.reserve(4000);
    std::atomic<bool> record=false;
    std::jthread receiver([&](std::stop_token s){receive(s,input.id,received,&record);});
    for(int i=0;i<100 && !received.ready;++i) std::this_thread::sleep_for(std::chrono::milliseconds(100));
    require(received.ready,"Probe receiver start");
    BYTE* bytes=nullptr; check(render->GetBuffer(capacity,&bytes),"Probe prime");
    check(render->ReleaseBuffer(capacity,AUDCLNT_BUFFERFLAGS_SILENT),"Probe release prime");
    check(client->Start(),"Probe start");
    struct Stop { IAudioClient* p; ~Stop(){p->Stop();} } stop{client.Get()};
    LARGE_INTEGER frequency; QueryPerformanceFrequency(&frequency);
    uint32_t rng=0x1729abcd; auto start=std::chrono::steady_clock::now();
    unsigned minPadding=UINT_MAX,maxPadding=0;
    for(;;) {
        double seconds=std::chrono::duration<double>(std::chrono::steady_clock::now()-start).count();
        if(seconds>=11) break;
        if(seconds>=1) record=true;
        UINT32 padding=0; check(client->GetCurrentPadding(&padding),"Probe padding");
        const auto n=capacity-padding;
        if(n) {
            check(render->GetBuffer(n,&bytes),"Probe buffer"); auto samples=reinterpret_cast<float*>(bytes);
            LARGE_INTEGER qpc; QueryPerformanceCounter(&qpc);
            if(record) {
                sent.packets.push_back({static_cast<uint64_t>(static_cast<long double>(qpc.QuadPart)*10000000/frequency.QuadPart),n,0,sent.audio.size()});
                minPadding=std::min(minPadding,padding); maxPadding=std::max(maxPadding,padding);
            }
            for(unsigned i=0;i<n;++i) {
                rng=rng*1664525u+1013904223u;
                float value=seconds>=1 && seconds<9 ? (static_cast<float>(rng>>8)/8388608.0f-1.0f)*0.02f:0.0f;
                samples[i*2]=value; samples[i*2+1]=value;
                if(record) sent.audio.push_back(value);
            }
            check(render->ReleaseBuffer(n,0),"Probe release");
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }
    record=false; receiver.request_stop(); receiver.join();
    saveRecording(folder/L"raw.pcmq",sent); saveRecording(folder/L"processed.pcmq",received);
    std::ofstream(folder/L"endpoints.txt")<<"raw: synthetic samples at render submission QPC (not playback timestamp)\nprocessed: CABLE Output capture\n";
    std::ofstream report(folder/L"probe.txt");
    report<<"render_capacity_frames="<<capacity<<" padding_min="<<minPadding<<" padding_max="<<maxPadding<<"\n";
    std::cout<<"PROBE SAVED; render capacity="<<capacity<<" queued frames="<<minPadding<<".."<<maxPadding<<"\n";
}
static void selfTest() {
    const std::vector<mic::Device> choices={{L"one",L"HyperX A"},{L"two",L"HyperX B"}};
    require(mic::preferredDevice(choices,L"two",L"HyperX")==1,"Saved endpoint wins over name hint");
    require(mic::preferredDevice(choices,L"missing",L"HyperX")==-1,"Missing endpoint must not switch microphones");
    require(mic::preferredDevice(choices,L"",L"HyperX")==0,"First-run device suggestion");
    mic::TagClock clock;
    unsigned frames=clock.take(0.014,0,480);
    require(frames==480 && clock.pending>191.9,"TAG late wake retains clock debt");
    for(int i=0;i<43;++i) frames+=clock.take(0.002,0,480);
    require(std::abs(static_cast<double>(frames)+clock.pending-4800)<0.001,"TAG elapsed time is not discarded");
    mic::Ring<8> ring;
    float in[]={0,1,2,3,4,5,6,7}, out[8]{};
    require(ring.push(in,8),"fill"); require(!ring.push(in,1),"reject overflow");
    require(ring.pop(out,5) && out[4]==4,"read order");
    require(ring.push(in,5),"wrap push"); require(ring.pop(out,8),"wrap pop");
    require(out[0]==5 && out[2]==7 && out[3]==0 && out[7]==4,"wrap order");
    require(!ring.pop(out,1),"reject underflow");
    ring.push(in,8); require(ring.trim(3)==5,"bounded latency discard");
    require(ring.pop(out,3) && out[0]==5,"discard order");
    // Concurrent sequence check catches overwrite/order defects across repeated wraps.
    mic::Ring<1024> concurrent;
    std::thread producer([&]{ for(int i=0;i<200000;++i) { float v=static_cast<float>(i); while(!concurrent.push(&v,1)) std::this_thread::yield(); } });
    bool ordered=true;
    for(int i=0;i<200000;++i) { float v=0; while(!concurrent.pop(&v,1)) std::this_thread::yield(); if(v!=static_cast<float>(i)) ordered=false; }
    producer.join(); require(ordered,"SPSC sequence");
    for(double mismatch:{-0.001,0.001}) {
        mic::Drift drift; double queue=960, correction=0;
        for(int i=0;i<6000;++i) { queue+=4800*(mismatch-correction); correction=drift.update(queue-960); }
        require(std::abs(queue-960)<20 && std::abs(correction-mismatch)<0.0001,"clock drift convergence");
    }
    std::cout<<"PASS: bounded queue, wrapping, concurrent ordering, +/-1000 ppm drift, TAG late-wake clock\n";
}
static void protocolTest() {
    {
        GUID generation{},old{};CoCreateGuid(&generation);CoCreateGuid(&old);
        mic::TagRecoveryBudget budget{generation,1};
        require(!budget.take(old) && budget.used==0,"Old generation cannot consume recovery");
        require(budget.take(generation),"Recovery consumes durable budget");
        mic::TagRecoveryBudget restored{};memcpy(&restored,&budget,sizeof(budget));
        require(restored.take(generation) && restored.take(generation) && !restored.take(generation),"Deserialized budget retains exactly three retries");
        restored.missingAt=100;restored.refresh();restored.refresh();
        require(restored.current(generation) && restored.used==0 && restored.missingAt==0,"Repeated launch refresh retains the pending worker generation");
        require(!restored.take(old) && restored.take(generation),"Refreshed recovery accepts only its current generation");
        restored.active=0;restored.refresh();
        require(!restored.take(generation),"Stop cancels delayed recovery even with tickets left");
    }
    {
        using ThinAudioGateway::VirtualLineDesc;
        auto line=[](unsigned id,bool capture,const wchar_t* name){VirtualLineDesc l{};l.Id=id;l.Capture=capture;l.Type=capture?ThinAudioGateway::VLT_Microphone:ThinAudioGateway::VLT_Headphones;wcscpy_s(l.KsName,name);return l;};
        std::vector<VirtualLineDesc> lines={line(1,true,L"TAG Microphone"),line(2,false,L"TAG Speakers"),line(9,true,L"Other")};
        const auto repair=mic::tagLineRepair(lines,1,2);
        require(repair.microphone==3 && repair.headphones==4 && repair.remove==std::vector<unsigned>{1},"Explicit repair removes only exact default input, migrates reserved IDs and preserves foreign lines");
        auto legacy=std::vector<VirtualLineDesc>{line(1,true,L"MicNoize Microphone"),line(2,false,L"MicNoize Headphones")};
        require(mic::tagLegacyRestartSafe(legacy),"Legacy rollback may restart over only its own lines");
        require(!mic::tagLegacyRestartSafe(lines),"Legacy rollback refuses foreign/default devices");
        const auto migrated=mic::tagLineRepair(legacy,1,2);
        require(migrated.microphone==3 && migrated.headphones==4 && migrated.remove.size()==2,"Explicit repair migrates both legacy own lines");
        auto stable=std::vector<VirtualLineDesc>{line(2,false,L"TAG Speakers"),line(3,true,L"MicNoize Microphone"),line(4,false,L"MicNoize Headphones")};
        require(mic::tagLegacyRestartSafe(stable),"Hash-pinned published legacy core preserves TAG defaults with a unique own input");
        auto unsafeLegacy=stable;unsafeLegacy.push_back(line(9,true,L"MicNoiseReducer Old"));
        require(!mic::tagLegacyRestartSafe(unsafeLegacy),"Legacy rollback refuses obsolete lines it would delete");
        const auto repeated=mic::tagLineRepair(stable,3,4);
        require(repeated.remove.empty() && repeated.microphone==3 && repeated.headphones==4,"Repeated line repair preserves stable microphone and headphones");
        legacy.push_back(line(3,true,L"MicNoize Microphone"));bool ambiguous=false;
        require(!mic::tagLegacyRestartSafe(legacy),"Legacy rollback refuses ambiguous capture identity");
        try{mic::tagLineRepair(legacy,1,2);}catch(const std::exception&){ambiguous=true;}
        require(ambiguous,"Line repair refuses ambiguous own identity before deletion");
        require(mic::tagLineIdentity(lines,true,3)==3,"Reboot restores saved line, not max ID plus one");
        std::reverse(lines.begin(),lines.end());
        require(mic::tagLineIdentity(lines,true,3)==3,"Driver enumeration order does not change identity");
        require(mic::tagLineIdentity({},true,0)==3,"Fresh allocation avoids driver startup IDs");
        require(mic::tagLineIdentity(lines,false,0,3)==4,"Headphone reservation avoids microphone and defaults");
        lines.push_back(line(3,true,L"MicNoize Microphone"));
        wcscpy_s(lines.back().EpName,L"User renamed it");
        require(mic::tagLineIdentity(lines,true,3)==3,"Reuse healthy renamed endpoint line");
        auto rejected=[&](unsigned saved){try{mic::tagLineIdentity(lines,true,saved);return false;}catch(const std::exception&){return true;}};
        require(rejected(7),"Saved and existing identity disagreement rejected");
        lines.back()=line(3,true,L"Other microphone");
        require(rejected(3),"Occupied saved ID rejected instead of migrating endpoint");
        lines.back()=line(3,true,L"MicNoize Microphone");lines.push_back(line(4,true,L"MicNoize Microphone"));
        require(rejected(3),"Duplicate own topology rejected");
    }
    {
        const auto previous=mic::environment(L"MNR_TAG_HOST_PATH");
        _wputenv_s(L"MNR_TAG_HOST_PATH",L"C:/Mic Noize/current/../current/mic_tag_host.exe");
        const auto path=mic::tagHostPath();_wputenv_s(L"MNR_TAG_HOST_PATH",previous.c_str());
        require(path.native()==L"C:\\Mic Noize\\current\\mic_tag_host.exe","Host ownership normalizes Windows separators and lexical components");
    }
    require(mic::tagHostMode(L" --stop \t")==L"--stop","Host tolerates PowerShell trailing whitespace");
    require(mic::tagHostMode(L"\"--task-start\" ")==L"--task-start","Host accepts quoted command");
    require(mic::tagHostMode(L"  ").empty(),"Host default command");
    bool invalidArgs=false;try{mic::tagHostMode(L"--stop extra");}catch(const std::exception&){invalidArgs=true;}
    require(invalidArgs,"Host rejects extra command arguments");
    require(mic::tagTransientStartup("driver HRESULT 0x80070490"),"late driver retries");
    require(!mic::tagTransientStartup("access HRESULT 0x80070005"),"access denied does not retry as missing driver");
    require(!mic::tagTransientStartup("API DLL HRESULT 0x8007007E"),"Missing runtime files require repair, not endless driver polling");
    GUID host{},first{},second{};require(SUCCEEDED(CoCreateGuid(&host)) && SUCCEEDED(CoCreateGuid(&first)) && SUCCEEDED(CoCreateGuid(&second)),"IPC test GUIDs");
    mic::TagSession session{host};mic::TagPacketV2 packet;packet.host=host;packet.connection=first;packet.request=1;packet.deadline=1500;
    require(session.accept(packet,1,1000),"IPC connect");
    require(!session.accept(packet,1,1000),"IPC duplicate connect rejected");
    packet.request=2;packet.frames=1;packet.samples[0]=0.25f;
    require(session.accept(packet,2,1100),"IPC valid audio");
    require(!session.accept(packet,2,1100),"IPC duplicate audio rejected");
    packet.request=3;packet.deadline=1099;
    require(!session.accept(packet,2,1100),"IPC expired request rejected");
    packet.deadline=1500;packet.bytes=0;
    require(!session.accept(packet,2,1100),"IPC size rejected");packet.bytes=sizeof(packet);
    packet.version=1;require(!session.accept(packet,2,1100),"IPC old version rejected");packet.version=2;
    packet.host=second;require(!session.accept(packet,2,1100),"IPC old host rejected");packet.host=host;
    packet.frames=16385;require(!session.accept(packet,2,1100),"IPC oversized block rejected before samples");packet.frames=1;
    packet.samples[0]=std::numeric_limits<float>::quiet_NaN();require(!session.accept(packet,2,1100),"IPC NaN rejected");packet.samples[0]=0;
    packet.connection=second;packet.request=1;packet.frames=0;
    require(session.accept(packet,1,1100),"IPC new connection");
    packet.connection=first;packet.request=4;
    require(!session.accept(packet,3,1100) && session.connection==second,"IPC old Stop cannot close new connection");
    packet.connection=second;packet.request=2;packet.frames=8193;
    require(!session.accept(packet,2,1100,true),"IPC headphone frame bound");
    packet.frames=0;require(session.accept(packet,4,1100,true),"IPC headphone status");
    session.reply(packet);require(packet.ack==2 && packet.ackConnection==second && packet.ackHost==host,"IPC response identity");

    const auto name=L"Local\\MicNoize.ProtocolCheck."+std::to_wstring(GetCurrentProcessId());
    mic::TagLink server(true,false,name.c_str());
    {mic::TagLink::Lock lock(server,100);*server.packet={};server.packet->host=host;}
    mic::TagLink client(false,false,name.c_str());
    std::string serverError;
    std::jthread responder([&] {
        try {
            require(WaitForSingleObject(server.request,1000)==WAIT_OBJECT_0,"IPC test request");
            {mic::TagLink::Lock lock(server,100);server.packet->ackHost=host;server.packet->ackConnection=server.packet->connection;server.packet->ack=0;}
            SetEvent(server.response);Sleep(30);
            {mic::TagLink::Lock lock(server,100);server.packet->result=0;mic::TagSession{host}.reply(*server.packet);}
            SetEvent(server.response);
        }catch(const std::exception& error){serverError=error.what();}
    });
    const auto started=GetTickCount64();const auto result=client.command(1);responder.join();
    require(serverError.empty() && result==0 && GetTickCount64()-started>=25,"IPC ignores stale response event");
    bool expired=false;try{client.command(4);}catch(...){expired=true;}
    require(expired && !client.usable(),"IPC timeout poisons old connection");
    {mic::TagLink::Lock lock(server,100);require(!mic::TagSession{host}.accept(*server.packet,4,GetTickCount64()),"IPC timed-out queued command rejected");}
    std::thread abandoned([&]{if(server.lock(100)){server.packet->frames=0xffffffff;server.packet->command=2;}});abandoned.join();
    {mic::TagLink::Lock lock(server,100);require(!mic::TagSession{host}.accept(*server.packet,2,GetTickCount64()),"IPC abandoned partial command rejected");}
    std::cout<<"PASS: IPC v2 bounds, NaN, old host/connection/Stop, duplicate and expired requests, stale responses, abandoned mutex\n";
}
int main(int argc,char** argv) {
    try {
        if(argc==4 && std::string(argv[1])=="--headphones-check") {
            const auto outputs=mic::devices(false);const auto index=std::stoi(argv[2]);
            require(index>=0 && static_cast<size_t>(index)<outputs.size(),"Headphone output index");
            mic::checkHeadphones(outputs[index].id,std::stoi(argv[3])!=0);return 0;
        }
        if(argc==2 && std::string(argv[1])=="--discord-capture") { mic::checkDiscordCapture(5); return 0; }
        if(argc==2 && std::string(argv[1])=="--self-test") { selfTest(); protocolTest(); return 0; }
        if(argc==2 && std::string(argv[1])=="--tag-level-check") { mic::checkTagLevel(); return 0; }
        if(argc==2 && std::string(argv[1])=="--tag-level-watch-check") { mic::checkTagLevelWatch(); return 0; }
        if(argc==2 && std::string(argv[1])=="--rvc-check") { mic::checkRvc(); return 0; }
        if((argc==6 || argc==7) && std::string(argv[1])=="--bench-afx") {
            std::ifstream input(mic::wide(argv[2]),std::ios::binary);
            require(static_cast<bool>(input),"Cannot open PCM packet recording");
            std::vector<float> samples;
            while(input.peek()!=EOF) {
                uint64_t qpc; uint32_t frames,flags;
                input.read(reinterpret_cast<char*>(&qpc),8); input.read(reinterpret_cast<char*>(&frames),4); input.read(reinterpret_cast<char*>(&flags),4);
                require(input && frames>0 && frames<=mic::rate && samples.size()+frames<=mic::rate*600,"Invalid PCM packet header");
                const auto start=samples.size(); samples.resize(start+frames);
                input.read(reinterpret_cast<char*>(samples.data()+start),frames*sizeof(float));
                require(static_cast<bool>(input),"Truncated PCM packet");
            }
            const int seconds=std::stoi(argv[3]); require(seconds>0 && seconds<=600,"Benchmark duration");
            mic::Config config; config.version=2; config.sdk=mic::projectRoot()/L"vendor/nvidia-afx-3.0.0"; config.cudaGraphs=std::stoi(argv[4]);
            mic::benchmarkAfx(config,samples,seconds,mic::wide(argv[5]),argc==7 && std::string(argv[6])=="churn");
            std::cout<<"Benchmark saved\n"; return 0;
        }
        auto inputs=mic::devices(true), outputs=mic::devices(false);
        if(argc==4 && std::string(argv[1])=="--monitor-check") {
            const int index=std::stoi(argv[2]);require(index>=0 && static_cast<size_t>(index)<inputs.size(),"input index");
            mic::Config config;config.input=inputs[index].id;config.tag=std::string(argv[3])=="TAG";
            if(config.tag)config.output=L"TAG";
            else {const int out=std::stoi(argv[3]);require(out>=0&&static_cast<size_t>(out)<outputs.size(),"output index");config.output=outputs[out].id;}
            config.version=2;config.sdk=mic::projectRoot()/L"vendor/nvidia-afx-3.0.0";
            config.tagSdk=mic::projectRoot()/L"vendor/tag-2.0.0.1903-demo";
            mic::Engine engine;mic::Monitor monitor(engine);engine.start(config);
            for(unsigned i=0;i<100 && engine.running() && engine.state==1;++i)std::this_thread::sleep_for(std::chrono::milliseconds(100));
            require(engine.running(),mic::utf8(engine.status()).c_str());
            for(unsigned cycle=0;cycle<2;++cycle){
                monitor.start(config.output);
                for(unsigned i=0;i<50 && monitor.state==1;++i)std::this_thread::sleep_for(std::chrono::milliseconds(100));
                require(monitor.state==2,mic::utf8(monitor.message()).c_str());
                std::this_thread::sleep_for(std::chrono::seconds(2));
                require(monitor.frames>48000,"Monitor did not render audio");
                require(engine.stats.outputActive,"Monitoring alone must activate the virtual microphone");
                engine.muted=true;std::this_thread::sleep_for(std::chrono::milliseconds(500));
                require(monitor.renderedPeak==0,"Monitor must honor final Mute");
                if(cycle==0)monitor.stop();else engine.stop();
                for(unsigned i=0;i<20 && monitor.state!=0;++i)std::this_thread::sleep_for(std::chrono::milliseconds(100));
                require(monitor.state==0,"Monitor must stop with the engine");
                std::cout<<"cycle="<<cycle<<" monitor_frames="<<monitor.frames<<" muted_peak="<<monitor.renderedPeak<<'\n';
                engine.muted=false;
            }
            require(engine.stats.underruns==0 && engine.stats.drops==0,"Monitoring disrupted the main audio path");
            std::cout<<"PASS: monitor-only client, toggle/restart, final Mute, engine Stop\n";return 0;
        }
        if(argc==3 && std::string(argv[1])=="--tag-reconnect") {
            // The host drops a producer after 50 ms without data. A stalled output thread must
            // re-register and continue instead of ending the session.
            const int index=std::stoi(argv[2]);require(index>=0 && static_cast<size_t>(index)<inputs.size(),"input index");
            mic::ensureTagHost();
            std::wstring endpoint;
            for(unsigned i=0;i<50 && endpoint.empty();++i) {
                for(const auto& d:mic::devices(true)) if(d.name.find(L"Thin Audio Gateway")!=std::wstring::npos) endpoint=d.id;
                if(endpoint.empty()) std::this_thread::sleep_for(std::chrono::milliseconds(100));
            }
            require(!endpoint.empty(),"TAG must already be active while processing is stopped");
            Received received;std::atomic<unsigned> phase=0;
            std::jthread receiver([&](std::stop_token s){receive(s,endpoint,received,nullptr,false,48000,2,&phase);});
            mic::Config config;config.input=inputs[index].id;config.output=L"TAG";config.tag=true;config.version=2;
            config.sdk=mic::projectRoot()/L"vendor/nvidia-afx-3.0.0";
            config.tagSdk=mic::projectRoot()/L"vendor/tag-2.0.0.1903-demo";
            mic::Engine engine;engine.start(config);
            for(unsigned i=0;i<100 && engine.running() && !engine.stats.outputActive;++i) std::this_thread::sleep_for(std::chrono::milliseconds(100));
            require(engine.stats.outputActive,"TAG client did not become active");
            phase=1;std::this_thread::sleep_for(std::chrono::seconds(2));
            require(engine.running(),mic::utf8(engine.status()).c_str());
            require(engine.stats.tagReconnects==0,"Reconnect counted without a stall");
            const unsigned stalls[]={120,400};
            for(unsigned i=0;i<2;++i) {
                phase=2+i;engine.testStallMs=stalls[i];
                std::this_thread::sleep_for(std::chrono::milliseconds(1500));
                require(engine.running(),mic::utf8(engine.status()).c_str());
                require(engine.stats.tagReconnects==i+1,"Stalled output thread did not re-register with the host");
                std::cout<<"stall_ms="<<stalls[i]<<" reconnects="<<engine.stats.tagReconnects<<" late_ticks="<<engine.stats.tagLateTicks
                    <<" underruns="<<engine.stats.underruns<<" drops="<<engine.stats.drops<<" discontinuities="<<engine.stats.discontinuities
                    <<" TAG_gaps="<<engine.stats.tagDriverGaps<<'\n'<<std::flush;
            }
            engine.stop();receiver.request_stop();receiver.join();
            require(received.error.empty(),received.error.c_str());
            for(unsigned i=1;i<4;++i) {
                std::cout<<"phase="<<i<<" frames="<<received.phaseFrames[i]<<" peak="<<received.phasePeak[i]<<'\n';
                require(received.phaseFrames[i]>48000,"Receiving client lost the stream around a stall");
            }
            std::cout<<"PASS: 120 ms and 400 ms output stalls survived; session continued with the same host\n";
            return 0;
        }
        if(argc==3 && std::string(argv[1])=="--persistent-tag") {
            const int index=std::stoi(argv[2]);require(index>=0 && static_cast<size_t>(index)<inputs.size(),"input index");
            mic::ensureTagHost();
            std::wstring endpoint;
            for(unsigned i=0;i<50 && endpoint.empty();++i) {
                for(const auto& d:mic::devices(true)) if(d.name.find(L"Thin Audio Gateway")!=std::wstring::npos) endpoint=d.id;
                if(endpoint.empty()) std::this_thread::sleep_for(std::chrono::milliseconds(100));
            }
            require(!endpoint.empty(),"TAG must already be active while processing is stopped");
            auto sameEndpoint=[&]{
                bool found=false;
                for(const auto& d:mic::devices(true)) if(d.id==endpoint) found=true;
                require(found,"TAG endpoint changed/disappeared after engine Stop");
            };
            Received received;std::atomic<unsigned> phase=0;
            std::jthread receiver([&](std::stop_token s){receive(s,endpoint,received,nullptr,false,48000,2,&phase);});
            mic::Config config;config.input=inputs[index].id;config.output=L"TAG";config.tag=true;config.version=2;
            config.sdk=mic::projectRoot()/L"vendor/nvidia-afx-3.0.0";
            config.tagSdk=mic::projectRoot()/L"vendor/tag-2.0.0.1903-demo";
            std::this_thread::sleep_for(std::chrono::seconds(2));
            for(unsigned cycle=0;cycle<2;++cycle) {
                phase=cycle*2+1;
                // Destroy the entire engine between cycles, while retaining the same
                // already-open WASAPI capture client throughout the test.
                {
                    mic::Engine engine;engine.start(config);
                    for(unsigned i=0;i<100 && engine.running() && !engine.stats.outputActive;++i) std::this_thread::sleep_for(std::chrono::milliseconds(100));
                    require(engine.stats.outputActive,"Persistent TAG client did not survive engine start");
                    std::this_thread::sleep_for(std::chrono::seconds(3));
                    require(engine.running(),mic::utf8(engine.status()).c_str());
                    require(engine.stats.underruns==0 && engine.stats.drops==0 && engine.stats.tagDriverGaps==0,"Persistent TAG audio gaps");
                    engine.stop();
                }
                std::this_thread::sleep_for(std::chrono::milliseconds(200));
                phase=cycle*2+2;sameEndpoint();
                std::this_thread::sleep_for(std::chrono::seconds(2));
            }
            receiver.request_stop();receiver.join();
            require(received.error.empty(),received.error.c_str());
            for(unsigned i=0;i<5;++i) {
                require(received.phaseFrames[i]>48000,"Existing client stopped receiving frames");
                if(i%2==0)require(received.phasePeak[i]==0,"Stopped engine must produce exact silence");
                std::cout<<"phase="<<i<<" frames="<<received.phaseFrames[i]<<" peak="<<received.phasePeak[i]<<'\n';
            }
            std::cout<<"PASS: same endpoint "<<mic::utf8(endpoint)<<", continuously open client, two engine lifetimes, stopped silence\n";
            return 0;
        }
        if(argc==3 && std::string(argv[1])=="--lifecycle-tag") {
            const int index=std::stoi(argv[2]); require(index>=0 && static_cast<size_t>(index)<inputs.size(),"input index");
            mic::Config config; config.input=inputs[index].id; config.output=L"TAG"; config.tag=true; config.version=2;
            config.sdk=mic::projectRoot()/L"vendor/nvidia-afx-3.0.0"; config.tagSdk=mic::projectRoot()/L"vendor/tag-2.0.0.1903-demo";
            mic::Engine engine; auto bad=config; bad.input=L"{0.0.1.00000000}.{00000000-0000-0000-0000-000000000000}";
            engine.start(bad);
            for(int i=0;i<100 && engine.running();++i) std::this_thread::sleep_for(std::chrono::milliseconds(100));
            require(!engine.running() && engine.status().find(L"Selected endpoint disconnected")!=std::wstring::npos,"Missing input must fail, never select another microphone");
            engine.stop();
            for(unsigned cycle=0;cycle<12;++cycle) {
                if(cycle%4==0) {
                    engine.start(config);
                    for(int i=0;i<100 && engine.running() && engine.stats.inputPeriodMs==0;++i) std::this_thread::sleep_for(std::chrono::milliseconds(100));
                    require(engine.running() && engine.stats.inputPeriodMs>0,"TAG start/restart failed");
                }
                std::wstring endpoint;
                for(int i=0;i<100 && endpoint.empty();++i) {
                    require(engine.running(),mic::utf8(engine.status()).c_str());
                    for(const auto& d:mic::devices(true)) if(d.name.find(L"Thin Audio Gateway")!=std::wstring::npos) endpoint=d.id;
                    if(endpoint.empty()) std::this_thread::sleep_for(std::chrono::milliseconds(100));
                }
                require(!endpoint.empty(),"TAG endpoint missing");
                const bool mute=cycle%4==1; engine.muted=mute;
                std::this_thread::sleep_for(std::chrono::milliseconds(100));
                const unsigned rate=cycle%2?44100:48000; const WORD channels=cycle%2?1:2;
                Received received;
                std::jthread receiver([&](std::stop_token s){receive(s,endpoint,received,nullptr,false,rate,channels);});
                std::this_thread::sleep_for(std::chrono::seconds(2));
                require(engine.stats.outputActive,"TAG output must report the connected client");
                require(!mute || engine.stats.outputPeak==0,"Muted output meter must show silence");
                receiver.request_stop(); receiver.join();
                require(received.error.empty(),received.error.c_str()); require(received.frames>rate,"Client reconnect lost audio");
                require(!mute || received.peak==0,"Mute leaked audio");
                require(engine.running(),mic::utf8(engine.status()).c_str());
                require(engine.stats.underruns==0 && engine.stats.drops==0 && engine.stats.tagDriverGaps==0,"Lifecycle audio gaps");
                std::cout<<"cycle="<<cycle+1<<" rate="<<rate<<" channels="<<channels<<" mute="<<mute<<" frames="<<received.frames<<" peak="<<received.peak<<"\n"<<std::flush;
                if(cycle%4==3) {
                    engine.stop();
                    require(!engine.stats.outputActive && engine.stats.outputPeak==0 && engine.stats.outputQueue==0,"Stopped output must clear active state and meters");
                }
                std::this_thread::sleep_for(std::chrono::milliseconds(500));
            }
            std::cout<<"PASS: missing input, recovery, 3 engine starts, 12 client connections, 48k stereo/44.1k mono, mute\n";
            return 0;
        }
        if(argc==4 && std::string(argv[1])=="--receive") {
            const int index=std::stoi(argv[2]), seconds=std::stoi(argv[3]);
            require(index>=0 && static_cast<size_t>(index)<inputs.size() && seconds>0 && seconds<=3600,"receiver arguments");
            Received received;
            std::jthread receiver([&](std::stop_token s){receive(s,inputs[index].id,received);});
            std::this_thread::sleep_for(std::chrono::seconds(seconds));
            receiver.request_stop(); receiver.join();
            require(received.error.empty(),received.error.c_str());
            require(received.frames>mic::rate,"No audio received");
            std::cout<<"received_frames="<<received.frames<<" peak="<<received.peak<<" (not recorded)\n";
            return 0;
        }
        if(argc==5 && std::string(argv[1])=="--probe-cable") {
            int out=std::stoi(argv[2]), in=std::stoi(argv[3]);
            require(out>=0 && static_cast<size_t>(out)<outputs.size() && in>=0 && static_cast<size_t>(in)<inputs.size(),"probe indices");
            probeCable(outputs[out],inputs[in],mic::wide(argv[4])); return 0;
        }
        const bool routes=argc==7 && std::string(argv[1])=="--record-routes";
        if((argc==5 && std::string(argv[1])=="--record-pair") || routes) {
            int a=std::stoi(argv[2]), b=std::stoi(argv[3]);
            require(a>=0 && b>=0 && static_cast<size_t>(a)<inputs.size() && static_cast<size_t>(b)<inputs.size(),"capture indices");
            int broadcast=routes?std::stoi(argv[4]):0, loopback=routes?std::stoi(argv[5]):0;
            require(!routes || (broadcast>=0 && static_cast<size_t>(broadcast)<inputs.size() && loopback>=0 && static_cast<size_t>(loopback)<outputs.size()),"comparison endpoint indices");
            const std::filesystem::path folder=mic::wide(argv[routes?6:4]);
            require(!std::filesystem::exists(folder),"Use a new recording directory");
            std::filesystem::create_directories(folder);
            std::atomic<bool> record=false; Received raw,processed,bc,lb;
            std::jthread t1([&](std::stop_token s){receive(s,inputs[a].id,raw,&record);});
            std::jthread t2([&](std::stop_token s){receive(s,inputs[b].id,processed,&record);});
            std::jthread t3,t4;
            if(routes) {
                t3=std::jthread([&](std::stop_token s){receive(s,inputs[broadcast].id,bc,&record);});
                t4=std::jthread([&](std::stop_token s){receive(s,outputs[loopback].id,lb,&record,true);});
            }
            auto ready=[&]{return raw.ready && processed.ready && (!routes || (bc.ready && lb.ready));};
            for(int i=0;i<100 && !ready();++i) std::this_thread::sleep_for(std::chrono::milliseconds(100));
            require(ready(),"All receivers must initialize");
            std::ofstream endpoints(folder/L"endpoints.txt");
            endpoints<<"raw: "<<mic::utf8(inputs[a].name)<<"\nprocessed: "<<mic::utf8(inputs[b].name)<<"\n";
            if(routes) endpoints<<"broadcast: "<<mic::utf8(inputs[broadcast].name)<<"\nloopback: "<<mic::utf8(outputs[loopback].name)<<"\n";
            std::ofstream(folder/L"ready").put('1');
            std::cout<<"READY; create "<<mic::utf8((folder/L"go").wstring())<<" to capture 14 seconds\n"<<std::flush;
            for(int i=0;i<3000 && !std::filesystem::exists(folder/L"go");++i) std::this_thread::sleep_for(std::chrono::milliseconds(100));
            require(std::filesystem::exists(folder/L"go"),"Recording trigger timed out");
            record=true; std::this_thread::sleep_for(std::chrono::seconds(14)); record=false;
            t1.request_stop(); t2.request_stop(); t1.join(); t2.join();
            if(routes) { t3.request_stop(); t4.request_stop(); t3.join(); t4.join(); }
            saveRecording(folder/L"raw.pcmq",raw); saveRecording(folder/L"processed.pcmq",processed);
            if(routes) { saveRecording(folder/L"broadcast.pcmq",bc); saveRecording(folder/L"loopback.pcmq",lb); }
            std::cout<<"SAVED raw_samples="<<raw.audio.size()<<" processed_samples="<<processed.audio.size()<<"\n";
            return 0;
        }
        if(argc==2 && std::string(argv[1])=="--list") {
            for(size_t i=0;i<inputs.size();++i) std::cout<<"INPUT "<<i<<" | "<<mic::utf8(inputs[i].name)<<" | "<<mic::utf8(inputs[i].id)<<"\n";
            for(size_t i=0;i<outputs.size();++i) std::cout<<"OUTPUT "<<i<<" | "<<mic::utf8(outputs[i].name)<<" | "<<mic::utf8(outputs[i].id)<<"\n";
            return 0;
        }
        const bool effects=argc>=5 && std::string(argv[1])=="--smoke-effects";
        const bool phrases=argc>=5 && std::string(argv[1])=="--smoke-phrases";
        if(argc<5 || (std::string(argv[1])!="--smoke" && !effects && !phrases)) {
            std::cerr<<"Usage: mic_check --list | --self-test | --smoke INPUT_INDEX OUTPUT_INDEX_OR_TAG SECONDS [VERSION] [BUFFER_MS] | --tag-reconnect INPUT_INDEX | --persistent-tag INPUT_INDEX | --receive CAPTURE_INDEX SECONDS | --bench-afx INPUT.pcmq SECONDS CUDA_GRAPHS OUTPUT.csv | --record-pair RAW_CAPTURE_INDEX PROCESSED_CAPTURE_INDEX NEW_DIRECTORY | --record-routes RAW_CAPTURE CABLE_CAPTURE BROADCAST_CAPTURE CABLE_RENDER NEW_DIRECTORY | --probe-cable CABLE_RENDER CABLE_CAPTURE NEW_DIRECTORY\n"; return 2;
        }
        const bool tag=std::string(argv[3])=="TAG";
        int input=std::stoi(argv[2]), output=tag?-1:std::stoi(argv[3]), seconds=std::stoi(argv[4]);
        require(input>=0 && static_cast<size_t>(input)<inputs.size(),"input index");
        require(tag || (output>=0 && static_cast<size_t>(output)<outputs.size()),"output index");
        require(seconds>=2 && seconds<=600,"duration 2..600 seconds");
        mic::Config config;
        config.input=inputs[input].id; config.output=tag?L"TAG":outputs[output].id;
        config.tag=tag; config.tagSdk=mic::projectRoot()/L"vendor/tag-2.0.0.1903-demo";
        config.version=argc>5?std::stoi(argv[5]):1;
        if(argc>6) config.bufferMs=static_cast<unsigned>(std::stoi(argv[6]));
        config.sdk=mic::projectRoot()/L"vendor/nvidia-afx-3.0.0";
        Received received; std::jthread receiver;
        if(!tag && outputs[output].name==L"CABLE Input (VB-Audio Virtual Cable)") {
            for(auto& endpoint:inputs) if(endpoint.name==L"CABLE Output (VB-Audio Virtual Cable)")
                receiver=std::jthread([&received,id=endpoint.id](std::stop_token token){receive(token,id,received);});
        }
        mic::Engine engine; engine.start(config);
        if(tag) {
            bool rejected=false;
            try {mic::Engine duplicate; duplicate.start(config);} catch(const std::exception& error) {
                rejected=std::string(error.what()).find("TAG is already running")!=std::string::npos;
            }
            require(rejected,"Concurrent TAG host must be rejected");
            for(int i=0;i<100 && !receiver.joinable();++i) {
                require(engine.running(),mic::utf8(engine.status()).c_str());
                for(const auto& endpoint:mic::devices(true)) if(endpoint.name.find(L"Thin Audio Gateway")!=std::wstring::npos) {
                    receiver=std::jthread([&received,id=endpoint.id](std::stop_token token){receive(token,id,received);}); break;
                }
                if(!receiver.joinable()) std::this_thread::sleep_for(std::chrono::milliseconds(100));
            }
            require(receiver.joinable(),"TAG endpoint did not appear");
        }
        std::atomic<unsigned> held=0;
        std::jthread sampler;
        if(effects || phrases) sampler=std::jthread([&](std::stop_token stop){while(!stop.stop_requested()){
            auto epoch=engine.effectEpoch.load();engine.heldSample=mic::packHeld(GetTickCount64(),epoch,held.load());
            std::this_thread::sleep_for(std::chrono::milliseconds(8));
        }});
        std::cout<<"Input: "<<mic::utf8(inputs[input].name)<<"\nOutput: "<<(tag?"TAG":mic::utf8(outputs[output].name))<<"\n";
        for(int i=0;i<seconds;++i) {
            if(effects) held=static_cast<unsigned>(i%4);
            if(phrases){
                held=(i==1||i==2||i==12)?4:(i==7||i==8)?8:0;
                if(i==13)engine.muted=true;
            }
            std::this_thread::sleep_for(std::chrono::seconds(1));
            if(!engine.running()) throw std::runtime_error(mic::utf8(engine.status()));
            if(phrases){
                if(i==1||i==2||i==12)require(engine.stats.phraseState==1,"Slow recording failed");
                if(i==3)require(engine.stats.phraseState==3,"Slow playback failed");
                if(i==6||i==11||i==13)require(engine.stats.phraseState==0,"Phrase did not finish/cancel");
                if(i==7||i==8)require(engine.stats.phraseState==2,"Fast recording failed");
                if(i==9)require(engine.stats.phraseState==4,"Fast playback failed");
            }
            if(effects && engine.stats.outputActive && !engine.muted) {
                require(engine.stats.boostActive==((held&1)!=0),"Live boost activation mismatch");
                require(engine.stats.pitchActive==((held&2)!=0),"Live pitch activation mismatch");
            }
            // Exercise live intensity and mute transitions without touching device settings.
            if(!phrases && i==seconds/3) engine.intensity=0.8f;
            if(!phrases && i==seconds/2) engine.muted=true;
            if(!phrases && i==seconds/2+1) {
                require(engine.stats.outputPeak==0,"Muted output meter must show silence");
                engine.muted=false; engine.intensity=1;
            }
        }
        if(sampler.joinable()){sampler.request_stop();sampler.join();engine.releaseEffects();}
        if(receiver.joinable()) {
            receiver.request_stop(); receiver.join();
            require(received.error.empty(),received.error.c_str()); require(received.frames>mic::rate,"No audio received at virtual microphone");
            std::cout<<"Virtual microphone received_frames="<<received.frames<<" peak="<<received.peak<<" (not recorded)\n";
        }
        engine.stop();
        auto& s=engine.stats;
        std::cout<<"frames="<<s.processed<<" last_ms="<<s.processMs<<" max_ms="<<s.maxProcessMs
            <<" underruns="<<s.underruns<<" drops="<<s.drops<<" capture_discontinuities="<<s.discontinuities
            <<" input_period_ms="<<s.inputPeriodMs<<" output_period_ms="<<s.outputPeriodMs
            <<" queue_ms="<<s.outputQueue/48.0<<" drift_ppm="<<s.driftPpm<<" reconfigure_ms="<<s.reconfigureMs<<"\n";
        if(tag) std::cout<<"TAG_gaps="<<s.tagDriverGaps<<" late_ticks="<<s.tagLateTicks<<" run_max_ms="<<s.maxRunMs<<" reset_max_ms="<<s.maxResetMs<<"\n";
        if(effects) std::cout<<"pitch_max_ms="<<s.pitchMaxMs<<" pitch_delay_ms="<<s.pitchDelayMs<<"\n";
        require(s.processed>100,"Not enough processed audio");
        require(s.underruns==0 && s.drops==0,"Audio gaps detected; increase the queue reserve or reduce GPU load");
        require(!tag || s.tagDriverGaps==0,"TAG driver reported gaps");
        std::cout<<"PASS: real capture -> NVIDIA -> selected output\n";
        return 0;
    } catch(const std::exception& e) { std::cerr<<"ERROR: "<<e.what()<<"\n"; return 1; }
}
