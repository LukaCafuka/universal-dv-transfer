// Bounded HDV transmission only. Requires STOP throughout; no tape commands.
#include "../native/windows.cpp"
#include <fstream>
#include <iostream>
static void diagnostic(void *, const uint8_t *bytes, int len) {
  if (bytes && len == INT_MIN) std::cout << (const char *)bytes << std::endl;
}
int main(int argc, char **argv) {
  if (argc != 3 && argc != 5 && argc != 6) return 2;
  try {
    std::ifstream file(argv[2], std::ios::binary);
    if (!file) throw std::runtime_error("HDV fixture unavailable");
    bool stride=argc==6 && std::strcmp(argv[5],"--stride")==0;
    if(argc==6 && !stride) return 2;
    std::vector<uint8_t> bytes((stride ? 192 : 188) * 256);
    // Exercise the same receive-to-output handover used by GUI preview.
    char error[2048]{};
    void *receiver = udvt_open(argv[1], 0, 3, 0, diagnostic, nullptr, error, sizeof(error));
    if (!receiver) throw std::runtime_error(error);
    for (int i=0;i<20;++i) udvt_poll(receiver, 25, error, sizeof(error));
    udvt_close(receiver);
    std::cout << "Closed Auto receiver before HDV output\n";
    Session s;
    s.hdv = true; s.bitrate = argc >= 5 ? (uint32_t)std::stoul(argv[3]) : 26700005;
    int blocks = argc >= 5 ? std::stoi(argv[4]) : 200;
    if (!s.bitrate || blocks < 1 || blocks > 2000) throw std::runtime_error("Invalid bounded test parameters");
    s.graph.create(CLSID_FilterGraph);
    s.builder.create(CLSID_CaptureGraphBuilder2);
    check(s.builder->SetFiltergraph(s.graph.p));
    check(s.graph->QueryInterface(IID_IMediaControl, (void **)s.control.out()));
    check(s.graph->QueryInterface(IID_IMediaEvent, (void **)s.events.out()));
    find_device(argv[1], s.device);
    check(s.graph->AddFilter(s.device.p, L"Deck"));
    check(s.device->QueryInterface(IID_IAMExtTransport, (void **)s.transport.out()));
    auto stopped = [&]() {
      long mode = 0; check(s.transport->get_Mode(&mode));
      if (mode != ED_MODE_STOP) throw std::runtime_error("Deck must remain stopped; test aborted");
    };
    stopped();
    auto report = [&]() {
      long value=0; HRESULT hr;
      for (long param : {ED_ERROR, ED_SERVO_LOCK}) {
        hr=s.transport->GetStatus(param, &value);
        std::printf("Deck status parameter=%ld HRESULT=0x%08lx value=%ld\n", param, (unsigned long)hr, value);
      }
      for (long param : {ED_BASE+940L, ED_BASE+941L}) {
        LPOLESTR text=nullptr; value=0;
        hr=s.transport->GetTransportBasicParameters(param, &value, &text);
        std::printf("Deck signal parameter=%ld HRESULT=0x%08lx value=%ld\n", param, (unsigned long)hr, value);
        CoTaskMemFree(text);
      }
      // AV/C STATUS only: input/output signal mode, never control commands.
      for (BYTE opcode : {BYTE(0x79), BYTE(0x78)}) {
        alignas(void*) BYTE response[512]{};
        response[0]=0x01; response[1]=0x20; response[2]=opcode; response[3]=0xff;
        long size=4;
        hr=s.transport->GetTransportBasicParameters(ED_BASE+920L, &size, (LPOLESTR*)response);
        std::printf("AV/C status opcode=%02x HRESULT=0x%08lx bytes=%ld response=", opcode, (unsigned long)hr, size);
        for(long n=0;n<size && n<16;++n) std::printf("%02x",response[n]);
        std::printf("\n");
      }
    };
    report();
    Com<IPin> sink;
    tape_input(s.device.p, true, diagnostic, nullptr, sink);
    s.source = new PushSource(false, true, diagnostic);
    s.sourceRef.p = s.source;
    check(s.graph->AddFilter(s.sourceRef.p, L"Source"));
    check(connect_hdv_output(&s, sink.p, diagnostic, nullptr));
    stopped();
    check(s.control->Run());
    auto start = GetTickCount64();
    for (int i = 0; i < blocks; ++i) {
      stopped();
      file.read((char *)bytes.data(), bytes.size());
      if (file.gcount() != (std::streamsize)bytes.size()) throw std::runtime_error("Expected complete TS block");
      for (size_t j=0;j<bytes.size();j+=(stride?192:188)) if (bytes[j+(stride?4:0)]!=0x47) throw std::runtime_error("Invalid TS alignment");
      if(stride) {
        Com<IMediaSample> sample;
        check(s.source->pin.allocator->GetBuffer(sample.out(),nullptr,nullptr,0));
        BYTE *p=nullptr; check(sample->GetPointer(&p));
        if(sample->GetSize()<(LONG)bytes.size()) throw std::runtime_error("Short allocator sample");
        memcpy(p,bytes.data(),bytes.size());
        check(sample->SetActualDataLength((LONG)bytes.size()));
        REFERENCE_TIME end=(s.ts_packets+256)*188ULL*8*10000000ULL/s.bitrate;
        check(sample->SetTime(&s.time,&end)); check(sample->SetSyncPoint(TRUE));
        check(s.source->pin.input->Receive(sample.p));
        s.time=end; s.ts_packets+=256;
      } else if (udvt_write(&s, bytes.data(), (int)bytes.size(), error, sizeof(error)) < 0) throw std::runtime_error(error);
      if (udvt_poll(&s, 10, error, sizeof(error)) < 0) throw std::runtime_error(error);
      if (i==blocks/2) report();
    }
    if (udvt_finish(&s, error, sizeof(error)) < 0) throw std::runtime_error(error);
    stopped();
    report();
    std::cout << "Delivered/drained " << blocks*256 << " HDV TS packets (" << blocks*256*188 << " bytes) in " << GetTickCount64()-start
              << " ms; deck remained STOP; no tape commands sent\n";
    return 0;
  } catch (HRESULT hr) {
    std::fprintf(stderr, "HDV send probe failed: 0x%08lx\n", (unsigned long)hr); return 1;
  } catch (const std::exception &error) {
    std::cerr << "HDV send probe failed: " << error.what() << std::endl; return 1;
  }
}
