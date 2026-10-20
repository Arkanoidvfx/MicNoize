#pragma once
#include <windows.h>
#include <algorithm>
#include <cmath>
#include <string_view>
#include <stdexcept>
#include "tag_version.h"

namespace mic {
inline constexpr unsigned tagProtocolVersion=MNR_TAG_PROTOCOL,tagHostBuild=MNR_TAG_BUILD;
struct TagRecoveryBudget {
    GUID generation{};
    unsigned active=0,used=0;
    ULONGLONG missingAt=0;
    bool current(const GUID& expected)const{return active && generation!=GUID_NULL && generation==expected;}
    bool take(const GUID& expected){if(!current(expected) || used>=3)return false;++used;return true;}
    void refresh(){used=0;missingAt=0;}
};
inline bool tagTransientStartup(std::string_view error) {
    return error.find("0x80070490")!=error.npos || error.find("0x8007048F")!=error.npos ||
        error.find("0x80070015")!=error.npos;
}
// v1 remains frozen. All v2 mapping, mutex and event names have their own suffix.
struct TagPacketV1 {unsigned version,command,frames,running,capacity,gaps;int result;float samples[16384];};
static_assert(sizeof(TagPacketV1)==65564);
struct TagPacketV2 {
    unsigned version=tagProtocolVersion,bytes=sizeof(TagPacketV2);
    GUID host{},connection{},ackHost{},ackConnection{};
    ULONGLONG request=0,ack=0,deadline=0;
    unsigned command=0,frames=0,running=0,capacity=0,gaps=0;
    int result=0;
    float samples[16384]{};
};
// Diagnostics only, beside the audio protocol: the host publishes the driver's capture buffer state
// after each loop pass; the engine reads it if present. An older host has no mapping and an older UI
// ignores it, so neither audio layout nor host/UI compatibility changes. Seqlock: odd while writing.
inline constexpr wchar_t tagBufferMapping[]=L"Local\\MicNoize.TagBuffer.v1";
struct TagBufferV1 {
    unsigned version=1,bytes=sizeof(TagBufferV1);
    volatile LONG sequence=0;
    unsigned running=0,bufferFrames=0,framesBuffered=0,clientPos=0,devicePos=0,overflows=0,underflows=0;
    ULONGLONG transferred=0;
};
inline void publishTagBuffer(TagBufferV1& to,const TagBufferV1& from) {
    InterlockedIncrement(&to.sequence);
    to.running=from.running;to.bufferFrames=from.bufferFrames;to.framesBuffered=from.framesBuffered;to.clientPos=from.clientPos;
    to.devicePos=from.devicePos;to.overflows=from.overflows;to.underflows=from.underflows;to.transferred=from.transferred;
    InterlockedIncrement(&to.sequence);
}
// False while the host is mid-write after a few tries, or for a layout this build does not know.
inline bool readTagBuffer(const TagBufferV1& from,TagBufferV1& out) {
    for(unsigned attempt=0;attempt<4;++attempt) {
        // Plain volatile loads: the reader maps the page read-only, so no interlocked write.
        const LONG before=from.sequence;
        if(before&1)continue;
        MemoryBarrier();
        out.version=from.version;out.bytes=from.bytes;out.running=from.running;out.bufferFrames=from.bufferFrames;
        out.framesBuffered=from.framesBuffered;out.clientPos=from.clientPos;out.devicePos=from.devicePos;
        out.overflows=from.overflows;out.underflows=from.underflows;out.transferred=from.transferred;
        MemoryBarrier();
        if(from.sequence==before)
            return out.version==1 && out.bytes==sizeof(TagBufferV1);
    }
    return false;
}
inline bool tagHeaderValid(const TagPacketV2& p,const GUID& host) {
    return p.version==tagProtocolVersion && p.bytes==sizeof(p) && host!=GUID_NULL && p.host==host;
}
struct TagSession {
    GUID host{},connection{};
    ULONGLONG lastRequest=0;
    bool accept(const TagPacketV2& p,unsigned op,ULONGLONG now,bool headphones=false) {
        if(!tagHeaderValid(p,host) || !op || op>4 || p.connection==GUID_NULL || !p.request ||
            p.deadline<now || p.deadline-now>500 || p.frames>(headphones?8192u:16384u) || (op!=2 && p.frames))return false;
        if(p.connection!=connection) {
            if(op!=1 || p.request!=1)return false;
        } else if(p.request<=lastRequest)return false;
        if(!headphones && op==2 && !std::all_of(p.samples,p.samples+p.frames,[](float v){return std::isfinite(v);}))return false;
        connection=p.connection;lastRequest=p.request;return true;
    }
    void reply(TagPacketV2& p)const {p.version=tagProtocolVersion;p.bytes=sizeof(p);p.host=host;p.ack=p.request;p.ackHost=host;p.ackConnection=p.connection;}
};
}
