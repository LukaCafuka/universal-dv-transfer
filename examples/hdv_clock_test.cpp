// Hardware-independent native timing regression; compile with a C++17 compiler.
#include "../native/hdv_output.h"
#include <algorithm>
#include <iostream>
#include <vector>
#include <fstream>
#include <string>

using Packet = std::array<uint8_t,188>;
static void require(bool ok, const char *message) {
  if(!ok) throw std::runtime_error(message);
}
static Packet packet(unsigned index, int pid=0x810) {
  Packet p{}; p.fill(uint8_t(index)); p[0]=0x47;
  p[1]=uint8_t(pid>>8); p[2]=uint8_t(pid); p[3]=uint8_t(0x10|(index&15)); return p;
}
static void pcr(Packet &p, uint64_t t, bool reset=false) {
  t%=((1ULL<<33)*300); const auto b=t/300, e=t%300;
  p[3]=uint8_t(0x30|(p[3]&15)); p[4]=7; p[5]=uint8_t(0x10|(reset?0x80:0));
  p[6]=uint8_t(b>>25); p[7]=uint8_t(b>>17); p[8]=uint8_t(b>>9); p[9]=uint8_t(b>>1);
  p[10]=uint8_t(((b&1)<<7)|0x7e|(e>>8)); p[11]=uint8_t(e);
}
struct Output {
  std::vector<Packet> packets;
  std::vector<uint64_t> times;
  uint64_t seconds=0, previous=0, end=0;
  void operator()(const uint8_t *bytes, uint64_t start, uint64_t stop) {
    require(stop>start && start>=end,"Invalid sample timeline"); end=stop;
    for(unsigned i=0;i<256;++i) {
      auto p=bytes+i*192;
      uint32_t sph=(uint32_t(p[0])<<24)|(uint32_t(p[1])<<16)|(uint32_t(p[2])<<8)|p[3];
      require(!(sph>>25) && ((sph>>12)&8191)<8000 && (sph&4095)<3072,"Invalid source packet header");
      uint64_t t=((sph>>12)&8191)*3072+(sph&4095);
      if(!times.empty() && t<previous) ++seconds;
      previous=t; times.push_back(t+seconds*24576000);
      Packet payload{}; std::copy_n(p+4,188,payload.begin()); packets.push_back(payload);
    }
  }
};
static void synthetic() {
  HdvOutputClock clock; Output output;
  auto emit=[&](const uint8_t *b,uint64_t a,uint64_t z){output(b,a,z);};
  std::vector<Packet> source;
  constexpr uint64_t origin=((1ULL<<33)*300)-540000;
  for(unsigned i=0;i<1601;++i) {
    auto p=packet(i);
    if(i==20) pcr(p,origin);
    if(i==720) pcr(p,origin+1080000);
    if(i==1420) pcr(p,origin+2160000);
    source.push_back(p); clock.push(p.data(),188,emit);
  }
  require(output.packets.size()==1280,"PCR lookahead did not retain the trailing packets");
  clock.finish(emit); clock.finish(emit);
  require(output.packets.size()==1792,"Final sample padding is wrong");
  require(std::equal(source.begin(),source.end(),output.packets.begin()),"TS payload changed");
  for(unsigned i=0;i<source.size();++i) {
    auto expected=uint64_t(i)*1080000*1024/(700*1125);
    require(output.times[i]<=expected && expected-output.times[i]<=1,"PCR spacing drifted");
  }
  require(output.packets.back()[1]==0x1f && output.packets.back()[2]==0xff,"Final padding is not null TS");
  // Change packet spacing, PCR origin and PID without padding between clips.
  HdvOutputClock playlist; Output joined;
  auto join=[&](const uint8_t *b,uint64_t a,uint64_t z){joined(b,a,z);};
  std::vector<Packet> clips;
  for(unsigned i=0;i<600;++i) {
    auto p=packet(i); if(i%200==0) pcr(p,2700000+uint64_t(i)*2700);
    clips.push_back(p); playlist.push(p.data(),188,join);
  }
  for(unsigned i=0;i<601;++i) {
    auto p=packet(i,0x134);
    if(i==0) pcr(p,900000,true);
    if(i==1) pcr(p,900000); // repeated boundary clock marker
    if(i==201) pcr(p,1440000);
    if(i==401) pcr(p,2250000); // slower packet spacing in this interval
    clips.push_back(p); playlist.push(p.data(),188,join);
  }
  playlist.finish(join);
  require(std::equal(clips.begin(),clips.end(),joined.packets.begin()),"Playlist payload/order changed");
  for(size_t i=1;i<joined.times.size();++i) require(joined.times[i]>joined.times[i-1],"Playlist clock reset");
  auto delta=joined.times[1001]-joined.times[801];
  require(delta>=737279 && delta<=737281,"Varying PCR spacing was not followed");
  bool rejected=false;
  try { HdvOutputClock missing; auto p=packet(0); missing.push(p.data(),188,emit); missing.finish(emit); }
  catch(const std::runtime_error &) { rejected=true; }
  require(rejected,"Missing timing was silently accepted");
}
int main(int argc,char **argv) {
  try {
    synthetic();
    if(argc==4) {
      std::ifstream input(argv[1],std::ios::binary), expected(argv[2],std::ios::binary);
      require(bool(input)&&bool(expected),"Fixtures not available");
      HdvOutputClock clock; Output output;
      auto emit=[&](const uint8_t *b,uint64_t a,uint64_t z){output(b,a,z);};
      unsigned count=unsigned(std::stoul(argv[3])); std::vector<Packet> source;
      for(unsigned i=0;i<count;++i) {
        Packet p{}; input.read((char*)p.data(),188); require(input.gcount()==188,"Short TS fixture");
        source.push_back(p); clock.push(p.data(),188,emit);
      }
      clock.finish(emit);
      require(std::equal(source.begin(),source.end(),output.packets.begin()),"Real capture TS changed");
      uint64_t seconds=0,previous=0;
      for(unsigned i=0;i<count;++i) {
        std::array<uint8_t,192> p{}; expected.read((char*)p.data(),192); require(expected.gcount()==192,"Short reference fixture");
        uint32_t h=(uint32_t(p[0])<<24)|(uint32_t(p[1])<<16)|(uint32_t(p[2])<<8)|p[3];
        uint64_t t=(h>>12)*3072+(h&4095); if(i && t<previous) ++seconds; previous=t;
        auto reference=t+seconds*24576000;
        // Two integer conversions can differ from exact rational interpolation
        // by at most two 24.576-MHz ticks (82 ns), without accumulated drift.
        if(!(output.times[i]<=reference && reference-output.times[i]<=2))
          throw std::runtime_error("Real capture timestamp differs at packet "+std::to_string(i)+": native="+
              std::to_string(output.times[i])+" reference="+std::to_string(reference));
      }
    }
    std::cout<<"HDV clock tests passed: PCR lookahead, variable spacing, wrap, reset/PID changes, payload preservation, final padding and missing timing\n";
  } catch(const std::exception &e) { std::cerr<<e.what()<<"\n"; return 1; }
}
