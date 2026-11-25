// Windows SDK diagnostic. Connects a stopped DV/HDV output graph without running
// it by default, delivering samples, or sending tape transport/record commands.
#include "../native/windows.cpp"
#include <iostream>

static void log_message(void *, const uint8_t *data, int length) {
  if (data && length == INT_MIN) std::cout << (const char *)data << std::endl;
}

// Hardware-independent regression using the Sony MSTape input description.
// FORMAT_None with only twelve bytes connected unsuccessfully on this driver.
static int test_hdv_format() {
  PushSource source(false, true);
  DWORD block[4]{4, 188, 192, 0x12345678};
  AM_MEDIA_TYPE advertised = source.pin.mt;
  advertised.formattype = {0x97e218b1, 0x1e5a, 0x498e, {0xa9,0x54,0xf9,0x62,0xcf,0xd9,0x8c,0xde}};
  advertised.bTemporalCompression = FALSE;
  advertised.cbFormat = sizeof(block);
  advertised.pbFormat = (BYTE *)block;
  if (!source.pin.use_hdv_type(advertised) ||
      source.pin.mt.formattype != advertised.formattype ||
      source.pin.mt.bTemporalCompression != FALSE ||
      source.pin.mt.cbFormat != sizeof(block) ||
      source.pin.mt.pbFormat == advertised.pbFormat ||
      std::memcmp(source.pin.mt.pbFormat, block, sizeof(block)))
    throw std::runtime_error("HDV negotiation did not retain the complete advertised format");
  block[3] = 0;
  DWORD retained;
  memcpy(&retained, source.pin.mt.pbFormat + 12, sizeof(retained));
  if (retained != 0x12345678) throw std::runtime_error("HDV format block was not owned");
  for (int field = 0; field < 3; ++field) {
    DWORD old = block[field]; block[field] = 0;
    if (source.pin.use_hdv_type(advertised)) throw std::runtime_error("Unsupported packet layout was accepted");
    block[field] = old;
  }
  advertised.lSampleSize = 188 * 256;
  if (source.pin.use_hdv_type(advertised)) throw std::runtime_error("Incorrect HDV sample size was accepted");
  std::cout << "HDV format regression passed: native GUID, complete owned block, packet-layout checks\n";
  return 0;
}

int main(int argc, char **argv) {
  if (argc == 2 && !std::strcmp(argv[1], "--self-test")) {
    try { return test_hdv_format(); }
    catch (const std::exception &error) { std::cerr << error.what() << std::endl; return 1; }
  }
  if ((argc != 3 && argc != 4) || (std::strcmp(argv[2], "pal") && std::strcmp(argv[2], "ntsc") && std::strcmp(argv[2], "hdv")) ||
      (argc == 4 && std::strcmp(argv[3], "--commit") && std::strcmp(argv[3], "--start"))) {
    std::cerr << "Usage: windows-output-probe <exact-device-id> pal|ntsc|hdv [--commit|--start]\n";
    return 2;
  }
  try {
    Session s;
    s.pal = !std::strcmp(argv[2], "pal");
    s.hdv = !std::strcmp(argv[2], "hdv");
    s.bitrate = s.hdv ? 26700000 : 0;
    s.graph.create(CLSID_FilterGraph);
    s.builder.create(CLSID_CaptureGraphBuilder2);
    check(s.builder->SetFiltergraph(s.graph.p));
    find_device(argv[1], s.device);
    check(s.graph->AddFilter(s.device.p, L"Deck"));
    Com<IPin> sink;
    tape_input(s.device.p, s.hdv, log_message, nullptr, sink);
    s.source = new PushSource(s.pal, s.hdv, log_message);
    s.sourceRef.p = s.source;
    check(s.graph->AddFilter(s.sourceRef.p, L"Source"));
    // Reproduce the old empty format block on the same input, then reconnect
    // using the corrected production source. This also tests failed cleanup.
    if (!s.hdv) {
      const DVINFO valid = s.source->pin.info;
      s.source->pin.info = {};
      s.source->pin.info.dwDVAAuxSrc = 0xffffffff;
      s.source->pin.info.dwDVAAuxCtl = 0xffffffff;
      HRESULT old = s.source->pin.Connect(sink.p, nullptr);
      if (SUCCEEDED(old)) {
        sink->Disconnect();
        s.source->pin.Disconnect();
        std::cout << "This driver also accepts the old format block\n";
      } else {
        char detail[100];
        std::snprintf(detail, sizeof(detail), "Old format rejected: 0x%08lx", (unsigned long)old);
        std::cout << detail << std::endl;
        if (s.source->pin.peer.p || s.source->pin.input.p || s.source->pin.allocator.p)
          throw HRESULT(E_UNEXPECTED);
      }
      s.source->pin.info = valid;
    }
    std::cout << "Corrected source: " << media_text(s.source->pin.mt) << std::endl;
    if (s.hdv) check(connect_hdv_output(&s, sink.p, log_message, nullptr));
    else check(s.builder->RenderStream(nullptr, &MEDIATYPE_Interleaved,
                                       s.sourceRef.p, nullptr, s.device.p));
    Com<IMediaFilter> filter;
    check(s.graph->QueryInterface(IID_IMediaFilter, (void **)filter.out()));
    FILTER_STATE state;
    check(filter->GetState(0, &state));
    if (state != State_Stopped) throw HRESULT(VFW_E_NOT_STOPPED);
    ALLOCATOR_PROPERTIES properties{};
    check(s.source->pin.allocator->GetProperties(&properties));
    if (properties.cbBuffer < (LONG)s.source->pin.mt.lSampleSize) throw HRESULT(E_FAIL);
    ALLOCATOR_PROPERTIES requirements{};
    HRESULT required = s.source->pin.input->GetAllocatorRequirements(&requirements);
    std::printf("Driver requirements hr=0x%08lx buffers=%ld bytes=%ld align=%ld prefix=%ld\n",
        (unsigned long)required, requirements.cBuffers, requirements.cbBuffer,
        requirements.cbAlign, requirements.cbPrefix);
    std::printf("Actual allocator buffers=%ld bytes=%ld align=%ld prefix=%ld\n",
        properties.cBuffers, properties.cbBuffer, properties.cbAlign, properties.cbPrefix);
    if (argc == 4) {
      HRESULT committed = s.source->pin.allocator->Commit();
      std::printf("Allocator Commit hr=0x%08lx\n", (unsigned long)committed);
      check(committed);
      Com<IMediaSample> sample;
      check(s.source->pin.allocator->GetBuffer(sample.out(), nullptr, nullptr, AM_GBF_NOWAIT));
      std::cout << "Acquired sample bytes=" << sample->GetSize() << "; not delivered\n";
      sample.out();
      check(s.source->pin.allocator->Decommit());
    }
    if (argc == 4 && std::strcmp(argv[3], "--commit")) {
      Com<IAMExtTransport> transport;
      check(s.device->QueryInterface(IID_IAMExtTransport, (void **)transport.out()));
      long mode = 0;
      check(transport->get_Mode(&mode));
      if (mode != ED_MODE_STOP) throw HRESULT(VFW_E_NOT_STOPPED);
      check(s.graph->QueryInterface(IID_IMediaControl, (void **)s.control.out()));
      HRESULT run = s.control->Run();
      std::printf("Output graph Run hr=0x%08lx\n", (unsigned long)run);
      check(run);
      check(s.control->Stop());
      check(filter->GetState(0, &state));
      if (state != State_Stopped) throw HRESULT(VFW_E_NOT_STOPPED);
      std::cout << "Output graph started/stopped; deck was STOP; no samples or tape commands sent\n";
    }
    std::cout << "Connected successfully; graph stopped; allocator bytes="
              << properties.cbBuffer << "; no samples or tape commands sent\n";
    return 0;
  } catch (HRESULT hr) {
    std::fprintf(stderr, "Stopped output probe failed: 0x%08lx\n", (unsigned long)hr);
    return 1;
  } catch (const std::exception &error) {
    std::cerr << error.what() << std::endl;
    return 1;
  }
}
