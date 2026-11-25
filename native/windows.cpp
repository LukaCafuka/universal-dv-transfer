#include "bridge.h"
#include "hdv_output.h"
#include <atomic>
#include <climits>
#include <cstdio>
#include <cstring>
#include <dshow.h>
#include <dvdmedia.h>
#include <stdexcept>
#include <string>
#include <vector>
#include <windows.h>
#include <setupapi.h>
#include <cfgmgr32.h>

// The MSDV and MSTape child interfaces can change when a deck switches format.
// Identify the physical 1394 parent, so recovery never selects a different deck.
static std::string physical_id(const std::wstring &path) {
  HDEVINFO set = SetupDiCreateDeviceInfoList(nullptr, nullptr);
  if (set == INVALID_HANDLE_VALUE) return {};
  SP_DEVICE_INTERFACE_DATA iface{}; iface.cbSize = sizeof(iface);
  SP_DEVINFO_DATA info{}; info.cbSize = sizeof(info);
  DWORD size = 0;
  std::string result;
  if (SetupDiOpenDeviceInterfaceW(set, path.c_str(), 0, &iface)) {
    SetupDiGetDeviceInterfaceDetailW(set, &iface, nullptr, 0, &size, nullptr);
    std::vector<BYTE> buffer(size);
    auto *detail = (SP_DEVICE_INTERFACE_DETAIL_DATA_W *)buffer.data();
    if (size >= sizeof(*detail)) {
      detail->cbSize = sizeof(*detail);
      if (SetupDiGetDeviceInterfaceDetailW(set, &iface, detail, size, nullptr, &info)) {
        DEVINST node = info.DevInst;
        for (int depth = 0; depth < 16; ++depth) {
          char id[MAX_DEVICE_ID_LEN]{};
          if (CM_Get_Device_IDA(node, id, sizeof(id), 0) != CR_SUCCESS) break;
          if (_strnicmp(id, "1394\\", 5) == 0) { result = std::string("firewire:") + id; break; }
          DEVINST parent;
          if (CM_Get_Parent(&parent, node, 0) != CR_SUCCESS) break;
          node = parent;
        }
      }
    }
  }
  SetupDiDestroyDeviceInfoList(set);
  return result;
}

// Small COM owner, keeping all DirectShow objects in the worker's MTA.
template <class T> struct Com {
  T *p = nullptr;
  ~Com() {
    if (p)
      p->Release();
  }
  T *operator->() const { return p; }
  T **out() {
    if (p) {
      p->Release();
      p = nullptr;
    }
    return &p;
  }
  void create(REFCLSID c) {
    HRESULT hr = CoCreateInstance(c, nullptr, CLSCTX_INPROC_SERVER, __uuidof(T),
                                  (void **)out());
    if (FAILED(hr))
      throw hr;
  }
};
static void check(HRESULT h) {
  if (FAILED(h))
    throw h;
}
static void fail(char *e, int cap, HRESULT hr) {
  if (cap > 0)
    std::snprintf(e, cap,
                  "DirectShow error 0x%08lx. Check the FireWire driver, VTR "
                  "mode and whether another application holds the deck.",
                  (unsigned long)hr);
}
static std::string utf8(const wchar_t *p) {
  if (!p)
    return {};
  int n = WideCharToMultiByte(CP_UTF8, 0, p, -1, nullptr, 0, nullptr, nullptr);
  std::string s(n, '\0');
  WideCharToMultiByte(CP_UTF8, 0, p, -1, s.data(), n, nullptr, nullptr);
  s.resize(n - 1);
  return s;
}
static std::string guid_text(const GUID &guid) {
  wchar_t value[64]{}; StringFromGUID2(guid, value, 64); return utf8(value);
}
static void trace(udvt_frame_cb cb, void *user, const std::string &message) {
  if (cb) cb(user, (const uint8_t *)message.c_str(), INT_MIN);
}
static std::string media_text(const AM_MEDIA_TYPE &mt) {
  auto text = "major=" + guid_text(mt.majortype) + " subtype=" + guid_text(mt.subtype) +
      " format=" + guid_text(mt.formattype) + " sample_size=" + std::to_string(mt.lSampleSize);
  if (mt.formattype == FORMAT_DvInfo && mt.pbFormat && mt.cbFormat >= sizeof(DVINFO)) {
    auto *info = (const DVINFO *)mt.pbFormat;
    char packs[180];
    std::snprintf(packs, sizeof(packs), " DVINFO=[%08lx,%08lx,%08lx,%08lx,%08lx,%08lx]",
        (unsigned long)info->dwDVAAuxSrc, (unsigned long)info->dwDVAAuxCtl,
        (unsigned long)info->dwDVAAuxSrc1, (unsigned long)info->dwDVAAuxCtl1,
        (unsigned long)info->dwDVVAuxSrc, (unsigned long)info->dwDVVAuxCtl);
    text += packs;
  }
  if (mt.subtype == MEDIASUBTYPE_MPEG2_TRANSPORT_STRIDE && mt.pbFormat && mt.cbFormat >= 12) {
    DWORD layout[3]; memcpy(layout, mt.pbFormat, sizeof(layout));
    text += " TS offset=" + std::to_string(layout[0]) + " packet=" + std::to_string(layout[1]) +
        " stride=" + std::to_string(layout[2]) + " format_bytes=" + std::to_string(mt.cbFormat);
  }
  return text;
}
struct Apartment {
  HRESULT hr;
  Apartment() : hr(CoInitializeEx(nullptr, COINIT_MULTITHREADED)) { check(hr); }
  ~Apartment() { CoUninitialize(); }
};
// Enumerating monikers must not instantiate an MSDV filter (some drivers crash
// on throwaway opens).
template <class Visit> static void each_device(Visit visit) {
  Com<ICreateDevEnum> en;
  en.create(CLSID_SystemDeviceEnum);
  Com<IEnumMoniker> it;
  HRESULT h =
      en->CreateClassEnumerator(CLSID_VideoInputDeviceCategory, it.out(), 0);
  check(h);
  if (h == S_FALSE)
    return;
  Com<IBindCtx> ctx;
  check(CreateBindCtx(0, ctx.out()));
  Com<IMoniker> mon;
  while (it->Next(1, mon.out(), nullptr) == S_OK) {
    Com<IPropertyBag> bag;
    if (FAILED(mon->BindToStorage(ctx.p, nullptr, IID_IPropertyBag,
                                  (void **)bag.out())))
      continue;
    VARIANT name, path;
    VariantInit(&name);
    VariantInit(&path);
    bag->Read(L"FriendlyName", &name, nullptr);
    bag->Read(L"DevicePath", &path, nullptr);
    std::wstring devicepath = path.vt == VT_BSTR ? path.bstrVal : L"";
    for (auto &c : devicepath)
      c = towlower(c);
    bool dv = devicepath.find(L"avc#") != std::wstring::npos;
    if (dv && name.vt == VT_BSTR) {
      LPOLESTR display = nullptr;
      check(mon->GetDisplayName(ctx.p, nullptr, &display));
      std::string id = physical_id(devicepath), label = utf8(name.bstrVal);
      VARIANT low, high; VariantInit(&low); VariantInit(&high);
      bag->Read(L"UniqueID_Low", &low, nullptr);
      bag->Read(L"UniqueID_High", &high, nullptr);
      if (id.empty() && low.vt == VT_I4 && high.vt == VT_I4 && (low.lVal || high.lVal)) {
        char guid[48];
        std::snprintf(guid, sizeof(guid), "firewire-guid:%08x%08x", (uint32_t)high.lVal, (uint32_t)low.lVal);
        id = guid;
      }
      VariantClear(&low); VariantClear(&high);
      if (id.empty()) id = utf8(display);
      CoTaskMemFree(display);
      bool done = visit(id, label, mon.p, ctx.p);
      VariantClear(&name);
      VariantClear(&path);
      if (done)
        return;
    } else {
      VariantClear(&name);
      VariantClear(&path);
    }
  }
}
static void find_device(const char *id, Com<IBaseFilter> &result,
                        std::string *resolved_id = nullptr, std::string *endpoint = nullptr) {
  each_device([&](const std::string &key, const std::string &, IMoniker *m,
                  IBindCtx *ctx) {
    if (key != id) {
      // Accept IDs emitted by older builds, which preferred MSTape's ROM
      // properties. New builds prefer the physical parent shared by MSDV/MSTape.
      bool legacy = false;
      Com<IPropertyBag> bag;
      if (SUCCEEDED(m->BindToStorage(ctx, nullptr, IID_IPropertyBag, (void **)bag.out()))) {
        VARIANT low, high; VariantInit(&low); VariantInit(&high);
        bag->Read(L"UniqueID_Low", &low, nullptr); bag->Read(L"UniqueID_High", &high, nullptr);
        if (low.vt == VT_I4 && high.vt == VT_I4) {
          char alias[48]; std::snprintf(alias, sizeof(alias), "firewire-guid:%08x%08x", (uint32_t)high.lVal, (uint32_t)low.lVal);
          legacy = _stricmp(alias, id) == 0;
        }
        VariantClear(&low); VariantClear(&high);
      }
      LPOLESTR display = nullptr;
      if (FAILED(m->GetDisplayName(ctx, nullptr, &display))) return false;
      legacy = legacy || utf8(display) == id;
      CoTaskMemFree(display);
      if (!legacy) return false;
    }
    check(
        m->BindToObject(ctx, nullptr, IID_IBaseFilter, (void **)result.out()));
    if (resolved_id) *resolved_id = key;
    if (endpoint) {
      LPOLESTR display = nullptr;
      check(m->GetDisplayName(ctx, nullptr, &display));
      *endpoint = utf8(display); CoTaskMemFree(display);
    }
    return true;
  });
  if (!result.p)
    throw HRESULT(VFW_E_NOT_FOUND);
}

// Sample Grabber is still present on desktop Windows; qedit.h is no longer in
// the SDK.
struct ISampleGrabberCB : IUnknown {
  virtual HRESULT STDMETHODCALLTYPE SampleCB(double, IMediaSample *) = 0;
  virtual HRESULT STDMETHODCALLTYPE BufferCB(double, BYTE *, long) = 0;
};
static const GUID IID_GrabCB = {
    0x0579154a,
    0x2b53,
    0x4994,
    {0xb0, 0xd0, 0xe7, 0x73, 0x14, 0x8e, 0xff, 0x85}};
struct ISampleGrabber : IUnknown {
  virtual HRESULT STDMETHODCALLTYPE SetOneShot(BOOL) = 0;
  virtual HRESULT STDMETHODCALLTYPE SetMediaType(const AM_MEDIA_TYPE *) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetConnectedMediaType(AM_MEDIA_TYPE *) = 0;
  virtual HRESULT STDMETHODCALLTYPE SetBufferSamples(BOOL) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetCurrentBuffer(long *, long *) = 0;
  virtual HRESULT STDMETHODCALLTYPE GetCurrentSample(IMediaSample **) = 0;
  virtual HRESULT STDMETHODCALLTYPE SetCallback(ISampleGrabberCB *, long) = 0;
};
static const GUID CLSID_Grab = {
    0xc1f400a0,
    0x3f08,
    0x11d3,
    {0x9f, 0x0b, 0x00, 0x60, 0x08, 0x03, 0x9e, 0x37}};
static const GUID CLSID_UDVTNullRenderer = {
    0xc1f400a4,
    0x3f08,
    0x11d3,
    {0x9f, 0x0b, 0x00, 0x60, 0x08, 0x03, 0x9e, 0x37}};
static const GUID IID_Grab = {0x6b652fff,
                              0x11fe,
                              0x4fce,
                              {0x92, 0xad, 0x02, 0x66, 0xb5, 0xd7, 0xc7, 0x8f}};
struct CaptureCallback : ISampleGrabberCB {
  std::atomic<ULONG> refs{1};
  udvt_frame_cb cb;
  void *user;
  bool automatic;
  uint8_t kind;
  std::atomic<unsigned long> samples{0};
  std::atomic<ULONGLONG> last_sample{GetTickCount64()};
  CaptureCallback(udvt_frame_cb c, void *u, bool a, bool hdv)
      : cb(c), user(u), automatic(a), kind(hdv ? 2 : 1) {}
  HRESULT STDMETHODCALLTYPE QueryInterface(REFIID i, void **p) override {
    if (!p)
      return E_POINTER;
    *p = nullptr;
    if (i == IID_IUnknown || i == IID_GrabCB) {
      *p = static_cast<ISampleGrabberCB *>(this);
      AddRef();
      return S_OK;
    }
    return E_NOINTERFACE;
  }
  ULONG STDMETHODCALLTYPE AddRef() override { return ++refs; }
  ULONG STDMETHODCALLTYPE Release() override {
    ULONG n = --refs;
    if (!n)
      delete this;
    return n;
  }
  HRESULT STDMETHODCALLTYPE SampleCB(double, IMediaSample *s) override {
    BYTE *p = nullptr;
    if (SUCCEEDED(s->GetPointer(&p))) {
      last_sample = GetTickCount64();
      auto number = ++samples;
      if (number <= 3) trace(cb, user, "Received sample " + std::to_string(number) + " bytes=" + std::to_string(s->GetActualDataLength()));
      if (automatic) {
        AM_MEDIA_TYPE *mt = nullptr;
        if (s->GetMediaType(&mt) == S_OK && mt) {
          kind = mt->majortype == MEDIATYPE_Interleaved ? 1 :
                 mt->majortype == MEDIATYPE_Stream ? 2 : 5;
          CoTaskMemFree(mt->pbFormat);
          if (mt->pUnk) mt->pUnk->Release();
          CoTaskMemFree(mt);
        }
        long len = s->GetActualDataLength();
        if (len > 0) {
          std::vector<uint8_t> bytes(1 + len); bytes[0] = kind;
          memcpy(bytes.data() + 1, p, len);
          cb(user, bytes.data(), (int)bytes.size());
        }
      } else cb(user, p, s->GetActualDataLength());
    }
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE BufferCB(double, BYTE *, long) override {
    return E_NOTIMPL;
  }
};

// Push source: MSDV's input pin supplies a standard memory allocator. No codec
// or transcode.
struct PushSource;
struct MediaTypes : IEnumMediaTypes {
  std::atomic<ULONG> refs{1};
  AM_MEDIA_TYPE mt{};
  bool read = false;
  MediaTypes(const AM_MEDIA_TYPE &t) : mt(t) {
    mt.pbFormat = (BYTE *)CoTaskMemAlloc(t.cbFormat);
    memcpy(mt.pbFormat, t.pbFormat, t.cbFormat);
  }
  ~MediaTypes() { CoTaskMemFree(mt.pbFormat); }
  HRESULT STDMETHODCALLTYPE QueryInterface(REFIID i, void **p) override {
    *p = nullptr;
    if (i != IID_IUnknown && i != IID_IEnumMediaTypes)
      return E_NOINTERFACE;
    *p = this;
    AddRef();
    return S_OK;
  }
  ULONG STDMETHODCALLTYPE AddRef() override { return ++refs; }
  ULONG STDMETHODCALLTYPE Release() override {
    auto n = --refs;
    if (!n)
      delete this;
    return n;
  }
  HRESULT STDMETHODCALLTYPE Next(ULONG n, AM_MEDIA_TYPE **p,
                                 ULONG *got) override {
    if (got)
      *got = 0;
    if (!n)
      return S_OK;
    if (read)
      return S_FALSE;
    *p = (AM_MEDIA_TYPE *)CoTaskMemAlloc(sizeof(mt));
    **p = mt;
    (*p)->pbFormat = (BYTE *)CoTaskMemAlloc(mt.cbFormat);
    memcpy((*p)->pbFormat, mt.pbFormat, mt.cbFormat);
    read = true;
    if (got)
      *got = 1;
    return n == 1 ? S_OK : S_FALSE;
  }
  HRESULT STDMETHODCALLTYPE Skip(ULONG n) override {
    if (!n)
      return S_OK;
    bool old = read;
    read = true;
    return !old && n == 1 ? S_OK : S_FALSE;
  }
  HRESULT STDMETHODCALLTYPE Reset() override {
    read = false;
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE Clone(IEnumMediaTypes **p) override {
    auto *v = new MediaTypes(mt);
    v->read = read;
    *p = v;
    return S_OK;
  }
};
struct PushPin : IPin {
  PushSource *source;
  Com<IPin> peer;
  Com<IMemInputPin> input;
  Com<IMemAllocator> allocator;
  AM_MEDIA_TYPE mt{};
  DVINFO info{};
  std::vector<BYTE> hdv_format;
  udvt_frame_cb callback;
  void *user;
  struct { DWORD offset, length, stride; } ts_format{4,188,192};
  explicit PushPin(PushSource *s, bool pal, bool hdv, udvt_frame_cb cb, void *u)
      : source(s), callback(cb), user(u) {
    mt.majortype = MEDIATYPE_Interleaved;
    mt.subtype = MEDIASUBTYPE_dvsd;
    mt.bFixedSizeSamples = TRUE;
    mt.bTemporalCompression = FALSE;
    mt.lSampleSize = pal ? 144000 : 120000;
    mt.formattype = FORMAT_DvInfo;
    mt.cbFormat = sizeof(info);
    mt.pbFormat = (BYTE *)&info;
    // MSDV validates the DVINFO connection block before seeing any frames.
    // Use Microsoft's DVCR/dvsd defaults, including consistent PAL/NTSC bits.
    // These describe the pin connection only; all embedded frame packs and
    // audio are transmitted unchanged, including 48 kHz/16-bit DV and DVCAM.
    info.dwDVAAuxSrc = pal ? 0xd1e030d0 : 0xd1c030cf;
    info.dwDVAAuxCtl = 0xffa0cf3f;
    info.dwDVAAuxSrc1 = pal ? 0xd1e03fd0 : 0xd1c03fcf;
    info.dwDVAAuxCtl1 = 0xffa0cf3f;
    info.dwDVVAuxSrc = pal ? 0xffe0ffff : 0xffc0ffff;
    info.dwDVVAuxCtl = pal ? 0xfffdc83f : 0xfffcc83f;
    if (hdv) {
      mt.majortype=MEDIATYPE_Stream;mt.subtype=MEDIASUBTYPE_MPEG2_TRANSPORT_STRIDE;
      mt.bTemporalCompression=TRUE;mt.lSampleSize=192*256;
      mt.formattype=FORMAT_None;mt.cbFormat=sizeof(ts_format);mt.pbFormat=(BYTE *)&ts_format;
    }
  }
  bool use_hdv_type(const AM_MEDIA_TYPE &candidate) {
    if (peer.p || candidate.majortype != MEDIATYPE_Stream ||
        candidate.subtype != MEDIASUBTYPE_MPEG2_TRANSPORT_STRIDE ||
        candidate.lSampleSize != 192 * 256 || !candidate.pbFormat ||
        candidate.cbFormat < sizeof(ts_format) || candidate.pUnk) return false;
    decltype(ts_format) layout;
    memcpy(&layout, candidate.pbFormat, sizeof(layout));
    // udvt_write emits four-byte source headers followed by 188 TS bytes.
    // Preserve the complete driver's block (including any extension bytes)
    // and GUID, but never claim a packet layout different from what we send.
    if (layout.offset != 4 || layout.length != 188 || layout.stride != 192) return false;
    hdv_format.assign(candidate.pbFormat, candidate.pbFormat + candidate.cbFormat);
    mt = candidate;
    mt.pbFormat = hdv_format.data();
    return true;
  }
  HRESULT STDMETHODCALLTYPE QueryInterface(REFIID i, void **p) override;
  ULONG STDMETHODCALLTYPE AddRef() override;
  ULONG STDMETHODCALLTYPE Release() override;
  HRESULT STDMETHODCALLTYPE Connect(IPin *p, const AM_MEDIA_TYPE *) override {
    if (!p) return E_POINTER;
    if (peer.p) return VFW_E_ALREADY_CONNECTED;
    const char *stage = "accept source media type";
    try {
      check(p->ReceiveConnection(this, &mt));
      peer.p = p;
      p->AddRef();
      stage = "query deck memory input";
      check(p->QueryInterface(IID_IMemInputPin, (void **)input.out()));
      stage = "get sample allocator";
      if (FAILED(input->GetAllocator(allocator.out())))
        allocator.create(CLSID_MemoryAllocator);
      ALLOCATOR_PROPERTIES wanted{}, actual{};
      if (FAILED(input->GetAllocatorRequirements(&wanted))) wanted = {};
      char requirements[180];
      std::snprintf(requirements, sizeof(requirements),
          "Tape output allocator requirements: buffers=%ld bytes=%ld align=%ld prefix=%ld",
          wanted.cBuffers, wanted.cbBuffer, wanted.cbAlign, wanted.cbPrefix);
      trace(callback, user, requirements);
      // Preserve downstream alignment, prefix and minimum buffer requirements,
      // as CBaseOutputPin does. The driver's allocator may be a KS allocator.
      if (wanted.cBuffers < 10) wanted.cBuffers = 10;
      if (wanted.cbBuffer < (LONG)mt.lSampleSize) wanted.cbBuffer = (LONG)mt.lSampleSize;
      if (wanted.cbAlign < 1) wanted.cbAlign = 1;
      stage = "configure sample allocator";
      check(allocator->SetProperties(&wanted, &actual));
      if (actual.cbBuffer < wanted.cbBuffer || actual.cBuffers < wanted.cBuffers ||
          actual.cbAlign < wanted.cbAlign || actual.cbPrefix < wanted.cbPrefix)
        throw HRESULT(E_FAIL);
      std::snprintf(requirements, sizeof(requirements),
          "Tape output allocator negotiated: buffers=%ld bytes=%ld align=%ld prefix=%ld",
          actual.cBuffers, actual.cbBuffer, actual.cbAlign, actual.cbPrefix);
      trace(callback, user, requirements);
      stage = "notify deck allocator";
      check(input->NotifyAllocator(allocator.p, FALSE));
      return S_OK;
    } catch (HRESULT hr) {
      char detail[160];
      std::snprintf(detail, sizeof(detail), "Tape output connection (%s): HRESULT=0x%08lx", stage, (unsigned long)hr);
      trace(callback, user, detail);
      if (peer.p) peer->Disconnect();
      Disconnect();
      return hr;
    } catch (...) {
      if (peer.p) peer->Disconnect();
      Disconnect();
      return E_FAIL;
    }
  }
  HRESULT STDMETHODCALLTYPE ReceiveConnection(IPin *,
                                              const AM_MEDIA_TYPE *) override {
    return E_UNEXPECTED;
  }
  HRESULT STDMETHODCALLTYPE Disconnect() override {
    if (allocator.p)
      allocator->Decommit();
    peer.out();
    input.out();
    allocator.out();
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE ConnectedTo(IPin **p) override {
    *p = peer.p;
    if (!*p)
      return VFW_E_NOT_CONNECTED;
    (*p)->AddRef();
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE ConnectionMediaType(AM_MEDIA_TYPE *p) override {
    if (!peer.p)
      return VFW_E_NOT_CONNECTED;
    *p = mt;
    p->pbFormat = (BYTE *)CoTaskMemAlloc(mt.cbFormat);
    memcpy(p->pbFormat, mt.pbFormat, mt.cbFormat);
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE QueryPinInfo(PIN_INFO *p) override;
  HRESULT STDMETHODCALLTYPE QueryDirection(PIN_DIRECTION *p) override {
    *p = PINDIR_OUTPUT;
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE QueryId(LPWSTR *p) override {
    *p = (LPWSTR)CoTaskMemAlloc(14);
    memcpy(*p, L"Output", 14);
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE QueryAccept(const AM_MEDIA_TYPE *p) override {
    return p->majortype == mt.majortype && p->subtype == mt.subtype ? S_OK
                                                                    : S_FALSE;
  }
  HRESULT STDMETHODCALLTYPE EnumMediaTypes(IEnumMediaTypes **p) override {
    *p = new MediaTypes(mt);
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE QueryInternalConnections(IPin **,
                                                     ULONG *) override {
    return E_NOTIMPL;
  }
  HRESULT STDMETHODCALLTYPE EndOfStream() override { return E_UNEXPECTED; }
  HRESULT STDMETHODCALLTYPE BeginFlush() override { return E_UNEXPECTED; }
  HRESULT STDMETHODCALLTYPE EndFlush() override { return E_UNEXPECTED; }
  HRESULT STDMETHODCALLTYPE NewSegment(REFERENCE_TIME, REFERENCE_TIME,
                                       double) override {
    return S_OK;
  }
};
struct Pins : IEnumPins {
  std::atomic<ULONG> refs{1};
  IPin *pin;
  bool read = false;
  explicit Pins(IPin *p) : pin(p) { pin->AddRef(); }
  ~Pins() { pin->Release(); }
  HRESULT STDMETHODCALLTYPE QueryInterface(REFIID i, void **p) override {
    *p = nullptr;
    if (i != IID_IUnknown && i != IID_IEnumPins)
      return E_NOINTERFACE;
    *p = this;
    AddRef();
    return S_OK;
  }
  ULONG STDMETHODCALLTYPE AddRef() override { return ++refs; }
  ULONG STDMETHODCALLTYPE Release() override {
    auto n = --refs;
    if (!n)
      delete this;
    return n;
  }
  HRESULT STDMETHODCALLTYPE Next(ULONG n, IPin **p, ULONG *got) override {
    if (got)
      *got = 0;
    if (!n)
      return S_OK;
    if (read)
      return S_FALSE;
    *p = pin;
    pin->AddRef();
    read = true;
    if (got)
      *got = 1;
    return n == 1 ? S_OK : S_FALSE;
  }
  HRESULT STDMETHODCALLTYPE Skip(ULONG n) override {
    if (!n)
      return S_OK;
    bool old = read;
    read = true;
    return !old && n == 1 ? S_OK : S_FALSE;
  }
  HRESULT STDMETHODCALLTYPE Reset() override {
    read = false;
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE Clone(IEnumPins **p) override {
    auto *v = new Pins(pin);
    v->read = read;
    *p = v;
    return S_OK;
  }
};
struct PushSource : IBaseFilter {
  std::atomic<ULONG> refs{1};
  PushPin pin;
  std::atomic<FILTER_STATE> state{State_Stopped};
  IFilterGraph *graph = nullptr;
  Com<IReferenceClock> clock;
  explicit PushSource(bool pal, bool hdv, udvt_frame_cb cb = nullptr, void *u = nullptr)
      : pin(this, pal, hdv, cb, u) {}
  HRESULT STDMETHODCALLTYPE QueryInterface(REFIID i, void **p) override {
    *p = nullptr;
    if (i != IID_IUnknown && i != IID_IBaseFilter && i != IID_IMediaFilter &&
        i != IID_IPersist)
      return E_NOINTERFACE;
    *p = static_cast<IBaseFilter *>(this);
    AddRef();
    return S_OK;
  }
  ULONG STDMETHODCALLTYPE AddRef() override { return ++refs; }
  ULONG STDMETHODCALLTYPE Release() override {
    auto n = --refs;
    if (!n)
      delete this;
    return n;
  }
  HRESULT STDMETHODCALLTYPE GetClassID(CLSID *p) override {
    *p = CLSID_NULL;
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE Stop() override {
    state = State_Stopped;
    if (pin.allocator.p)
      pin.allocator->Decommit();
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE Pause() override {
    HRESULT hr = pin.allocator.p ? pin.allocator->Commit() : S_OK;
    char detail[100];
    std::snprintf(detail, sizeof(detail), "Output source Pause/allocator Commit: HRESULT=0x%08lx", (unsigned long)hr);
    trace(pin.callback, pin.user, detail);
    if (SUCCEEDED(hr)) state = State_Paused;
    return hr;
  }
  HRESULT STDMETHODCALLTYPE Run(REFERENCE_TIME) override {
    HRESULT hr = pin.allocator.p ? pin.allocator->Commit() : S_OK;
    char detail[100];
    std::snprintf(detail, sizeof(detail), "Output source Run/allocator Commit: HRESULT=0x%08lx", (unsigned long)hr);
    trace(pin.callback, pin.user, detail);
    if (SUCCEEDED(hr)) state = State_Running;
    return hr;
  }
  HRESULT STDMETHODCALLTYPE GetState(DWORD, FILTER_STATE *p) override {
    *p = state.load();
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE SetSyncSource(IReferenceClock *p) override {
    clock.out();
    clock.p = p;
    if (p)
      p->AddRef();
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE GetSyncSource(IReferenceClock **p) override {
    *p = clock.p;
    if (*p)
      (*p)->AddRef();
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE EnumPins(IEnumPins **p) override {
    *p = new Pins(&pin);
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE FindPin(LPCWSTR id, IPin **p) override {
    *p = nullptr;
    if (wcscmp(id, L"Output"))
      return VFW_E_NOT_FOUND;
    *p = &pin;
    pin.AddRef();
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE QueryFilterInfo(FILTER_INFO *p) override {
    wcscpy(p->achName, L"Universal DV Source");
    p->pGraph = graph;
    if (graph)
      graph->AddRef();
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE JoinFilterGraph(IFilterGraph *p, LPCWSTR) override {
    graph = p;
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE QueryVendorInfo(LPWSTR *) override {
    return E_NOTIMPL;
  }
};
HRESULT PushPin::QueryInterface(REFIID i, void **p) {
  *p = nullptr;
  if (i != IID_IUnknown && i != IID_IPin)
    return E_NOINTERFACE;
  *p = this;
  AddRef();
  return S_OK;
}
ULONG PushPin::AddRef() { return source->AddRef(); }
ULONG PushPin::Release() { return source->Release(); }
HRESULT PushPin::QueryPinInfo(PIN_INFO *p) {
  p->pFilter = source;
  source->AddRef();
  p->dir = PINDIR_OUTPUT;
  wcscpy(p->achName, L"Output");
  return S_OK;
}
static void disconnect_filter(IGraphBuilder *graph, IBaseFilter *filter) {
  if (!graph || !filter) return;
  Com<IEnumPins> pins;
  if (SUCCEEDED(filter->EnumPins(pins.out()))) {
    Com<IPin> pin;
    while (pins->Next(1,pin.out(),nullptr)==S_OK) {
      Com<IPin> peer;
      if (SUCCEEDED(pin->ConnectedTo(peer.out()))) graph->Disconnect(peer.p);
      graph->Disconnect(pin.p);
    }
  }
  graph->RemoveFilter(filter);
}
struct Session {
  Apartment apartment;
  Com<IGraphBuilder> graph;
  Com<ICaptureGraphBuilder2> builder;
  Com<IMediaControl> control;
  Com<IMediaEvent> events;
  Com<IBaseFilter> device, grabFilter, nullFilter, sourceRef;
  Com<IAMExtTransport> transport;
  Com<IAMDroppedFrames> dropped;
  long previous_dropped = 0;
  unsigned long long frame_number = 0;
  Com<ISampleGrabber> grab;
  Com<CaptureCallback> callback;
  PushSource *source = nullptr;
  REFERENCE_TIME time = 0;
  bool pal = false;
  bool hdv = false;
  uint32_t bitrate = 0;
  uint64_t ts_packets = 0;
  HdvOutputClock hdv_clock;
  bool automatic = false;
  std::string device_id, endpoint;
  ULONGLONG checked_device = 0, playing_since = 0;
  ~Session() {
    if (control.p)
      control->Stop();
    if (grab.p)
      grab->SetCallback(nullptr, 0);
    if (source) {
      if (source->pin.peer.p)
        source->pin.peer->Disconnect();
      source->pin.Disconnect();
    } else {
      // Release the driver's pin/isochronous allocation before a replacement
      // receiver opens. This also runs after a partially connected graph fails.
      disconnect_filter(graph.p, nullFilter.p);
      disconnect_filter(graph.p, grabFilter.p);
      disconnect_filter(graph.p, device.p);
    }
  }
};
static void filter_pin(IBaseFilter *filter, PIN_DIRECTION direction, Com<IPin> &result) {
  Com<IEnumPins> pins; check(filter->EnumPins(pins.out()));
  Com<IPin> pin;
  while (pins->Next(1, pin.out(), nullptr) == S_OK) {
    PIN_DIRECTION actual;
    if (SUCCEEDED(pin->QueryDirection(&actual)) && actual == direction) {
      *result.out() = pin.p; pin->AddRef(); return;
    }
  }
  throw HRESULT(VFW_E_NOT_FOUND);
}
// Match the driver's recording input, not its capture/output pins. MSDV can
// still expose the physical Sony deck after its format selector changes, but
// cannot receive HDV. Never let Intelligent Connect insert a conversion path.
static void tape_input(IBaseFilter *filter, bool hdv, udvt_frame_cb cb, void *user,
                       Com<IPin> &result) {
  bool supports_dv = false, supports_hdv = false;
  Com<IEnumPins> pins; check(filter->EnumPins(pins.out()));
  Com<IPin> pin;
  while (pins->Next(1, pin.out(), nullptr) == S_OK) {
    PIN_DIRECTION direction;
    if (FAILED(pin->QueryDirection(&direction)) || direction != PINDIR_INPUT) continue;
    PIN_INFO info{}; std::string name;
    if (SUCCEEDED(pin->QueryPinInfo(&info))) {
      name = utf8(info.achName); if (info.pFilter) info.pFilter->Release();
    }
    Com<IEnumMediaTypes> types;
    if (FAILED(pin->EnumMediaTypes(types.out()))) continue;
    AM_MEDIA_TYPE *mt = nullptr;
    while (types->Next(1, &mt, nullptr) == S_OK) {
      bool dv = mt->majortype == MEDIATYPE_Interleaved && mt->subtype == MEDIASUBTYPE_dvsd;
      bool ts = mt->majortype == MEDIATYPE_Stream && mt->subtype == MEDIASUBTYPE_MPEG2_TRANSPORT_STRIDE;
      trace(cb, user, "Recording input pin " + name + " " + media_text(*mt));
      supports_dv |= dv; supports_hdv |= ts;
      if (!result.p && (hdv ? ts : dv)) { *result.out() = pin.p; pin->AddRef(); }
      CoTaskMemFree(mt->pbFormat); if (mt->pUnk) mt->pUnk->Release(); CoTaskMemFree(mt);
    }
  }
  if (result.p) return;
  if (hdv && supports_dv)
    throw std::runtime_error("HDV tape output was selected, but Windows exposes this deck through its DV-only (MSDV) input. Set HDV/DV SEL to HDV, turn HDV-to-DV conversion off, reconnect FireWire, then refresh devices. Keep the recording input on HDV/DV (i.LINK). No Record command or video was sent.");
  if (!hdv && supports_hdv)
    throw std::runtime_error("DV tape output was selected, but Windows exposes only the deck's HDV (MSTape) input. Set HDV/DV SEL to DV, reconnect FireWire, then refresh devices. Keep the recording input on HDV/DV (i.LINK). No Record command or video was sent.");
  throw std::runtime_error(hdv ? "This device exposes no HDV transport-stream recording input. Check the deck mode and Windows driver."
                               : "This device exposes no DV25 recording input. Check the deck mode and Windows driver.");
}
static HRESULT connect_hdv_output(Session *s, IPin *sink, udvt_frame_cb cb, void *user) {
  Com<IEnumMediaTypes> types; check(sink->EnumMediaTypes(types.out()));
  AM_MEDIA_TYPE *mt = nullptr;
  HRESULT result = VFW_E_TYPE_NOT_ACCEPTED;
  while (types->Next(1, &mt, nullptr) == S_OK) {
    bool supported = s->source->pin.use_hdv_type(*mt);
    CoTaskMemFree(mt->pbFormat); if (mt->pUnk) mt->pUnk->Release(); CoTaskMemFree(mt);
    if (!supported) continue;
    trace(cb, user, "Negotiating HDV recording input: " + media_text(s->source->pin.mt));
    result = s->graph->ConnectDirect(&s->source->pin, sink, &s->source->pin.mt);
    if (SUCCEEDED(result)) return result;
    s->graph->Disconnect(&s->source->pin); s->graph->Disconnect(sink);
  }
  return result;
}
// Explicit pin connections avoid Intelligent Connect inserting MPEG splitters
// or choosing a stale/incorrect stream pin during an MSDV/MSTape transition.
static HRESULT connect_hdv(Session *s, udvt_frame_cb cb, void *user) {
  Com<IPin> grab_in, grab_out, sink_in;
  filter_pin(s->grabFilter.p,PINDIR_INPUT,grab_in);
  filter_pin(s->grabFilter.p,PINDIR_OUTPUT,grab_out);
  filter_pin(s->nullFilter.p,PINDIR_INPUT,sink_in);
  Com<IEnumPins> pins; check(s->device->EnumPins(pins.out()));
  Com<IPin> source;
  HRESULT result=VFW_E_CANNOT_CONNECT;
  while (pins->Next(1,source.out(),nullptr)==S_OK) {
    PIN_DIRECTION direction;
    if (FAILED(source->QueryDirection(&direction)) || direction!=PINDIR_OUTPUT) continue;
    Com<IEnumMediaTypes> types;
    if (FAILED(source->EnumMediaTypes(types.out()))) continue;
    AM_MEDIA_TYPE *mt=nullptr;
    while (types->Next(1,&mt,nullptr)==S_OK) {
      bool hdv=mt->majortype==MEDIATYPE_Stream &&
        (mt->subtype==MEDIASUBTYPE_MPEG2_TRANSPORT || mt->subtype==MEDIASUBTYPE_MPEG2_TRANSPORT_STRIDE);
      if (hdv) {
        const char *stage="sample type";
        result=s->grab->SetMediaType(mt);
        if (SUCCEEDED(result)) { stage="device to grabber"; result=s->graph->ConnectDirect(source.p,grab_in.p,mt); }
        if (SUCCEEDED(result)) { stage="grabber to sink"; result=s->graph->ConnectDirect(grab_out.p,sink_in.p,mt); }
        char status[48];std::snprintf(status,sizeof(status)," HRESULT=0x%08lx",(unsigned long)result);
        trace(cb,user,"Direct HDV pin connection ("+std::string(stage)+") "+media_text(*mt)+status);
        if (FAILED(result)) {
          s->graph->Disconnect(source.p); s->graph->Disconnect(grab_in.p);
          s->graph->Disconnect(grab_out.p); s->graph->Disconnect(sink_in.p);
        }
      }
      CoTaskMemFree(mt->pbFormat); if(mt->pUnk)mt->pUnk->Release(); CoTaskMemFree(mt);
      if (hdv && SUCCEEDED(result)) return result;
    }
  }
  return result;
}
extern "C" int udvt_list(udvt_device_cb cb, void *u, char *e, int cap) {
  try {
    Apartment a;
    each_device([&](const std::string &id, const std::string &n, IMoniker *,
                    IBindCtx *) {
      cb(u, id.c_str(), n.c_str(), 1);
      return false;
    });
    return 0;
  } catch (HRESULT h) {
    fail(e, cap, h);
    return -1;
  } catch (...) {
    fail(e, cap, E_FAIL);
    return -1;
  }
}
extern "C" void *udvt_open(const char *id, int output, int pal, uint32_t bitrate,
                           udvt_frame_cb cb, void *u, char *e, int cap) {
  Session *s = nullptr;
  const char *stage = "create DirectShow graph";
  try {
    s = new Session;
    s->pal = pal == 1;
    s->hdv = pal == 2;
    s->bitrate = bitrate;
    if (output && s->hdv && !bitrate) throw HRESULT(E_INVALIDARG);
    s->graph.create(CLSID_FilterGraph);
    s->builder.create(CLSID_CaptureGraphBuilder2);
    check(s->builder->SetFiltergraph(s->graph.p));
    check(
        s->graph->QueryInterface(IID_IMediaControl, (void **)s->control.out()));
    check(s->graph->QueryInterface(IID_IMediaEvent, (void **)s->events.out()));
    stage = "bind selected FireWire device";
    s->automatic = !output && pal == 3;
    find_device(id, s->device, &s->device_id, &s->endpoint);
    if (!output) {
      bool supports_dv = false, supports_hdv = false;
      bool first_hdv = false, found = false;
      Com<IEnumPins> pins; check(s->device->EnumPins(pins.out()));
      Com<IPin> pin;
      while (pins->Next(1, pin.out(), nullptr) == S_OK) {
        PIN_DIRECTION direction;
        if (FAILED(pin->QueryDirection(&direction)) || direction != PINDIR_OUTPUT) continue;
        PIN_INFO info{}; std::string name;
        if (SUCCEEDED(pin->QueryPinInfo(&info))) {
          name = utf8(info.achName); if (info.pFilter) info.pFilter->Release();
        }
        Com<IEnumMediaTypes> types;
        if (FAILED(pin->EnumMediaTypes(types.out()))) continue;
        AM_MEDIA_TYPE *mt = nullptr;
        while (types->Next(1, &mt, nullptr) == S_OK) {
          bool dv = mt->majortype == MEDIATYPE_Interleaved && mt->subtype == MEDIASUBTYPE_dvsd;
          bool hdv = mt->majortype == MEDIATYPE_Stream &&
            (mt->subtype == MEDIASUBTYPE_MPEG2_TRANSPORT || mt->subtype == MEDIASUBTYPE_MPEG2_TRANSPORT_STRIDE);
          trace(cb, u, "Output pin " + name + " " + media_text(*mt));
          supports_dv |= dv; supports_hdv |= hdv;
          if (!found && (dv || hdv)) { first_hdv = hdv; found = true; }
          CoTaskMemFree(mt->pbFormat); if (mt->pUnk) mt->pUnk->Release(); CoTaskMemFree(mt);
        }
      }
      if (pal == 3) {
        if (!found) throw std::runtime_error("The driver exposes neither DV25 nor HDV transport-stream output pins.");
        s->hdv = first_hdv;
      } else if (s->hdv && !supports_hdv) {
        throw std::runtime_error("HDV capture requires an MPEG-2 transport-stream output pin, but this device exposes no HDV pin. Windows may still be using the DV (MSDV) driver. Set the deck's i.LINK output to HDV, disable HDV-to-DV conversion, reconnect the deck, and refresh the device list.");
      } else if (!s->hdv && !supports_dv) {
        throw std::runtime_error("DV capture was selected, but the driver exposes no DV25 interleaved output pin. Choose HDV/Auto or check the deck's output setting.");
      }
      trace(cb, u, std::string("Selected capture type: ") + (s->hdv ? "HDV MPEG-2 TS" : "DV25 interleaved"));
    }
    stage = "add FireWire device to graph";
    check(s->graph->AddFilter(s->device.p, L"DV deck"));
    s->device->QueryInterface(IID_IAMExtTransport, (void **)s->transport.out());
    if (s->transport.p) {
      long mode = 0;
      if (SUCCEEDED(s->transport->get_Mode(&mode))) trace(cb, u, "Tape transport mode=" + std::to_string(mode) +
          (mode == ED_MODE_PLAY ? " (PLAY)" : mode == ED_MODE_STOP ? " (STOP)" : mode == ED_MODE_FREEZE ? " (PAUSE)" : ""));
    }
    if (output) {
      stage = "check deck recording input format";
      Com<IPin> sink;
      tape_input(s->device.p, s->hdv, cb, u, sink);
      stage = "add tape-output source to graph";
      s->source = new PushSource(s->pal, s->hdv, cb, u);
      s->sourceRef.p = s->source;
      check(s->graph->AddFilter(s->sourceRef.p, L"DV source"));
      trace(cb, u, "Tape output media type: " + media_text(s->source->pin.mt));
      stage = "connect source to deck input";
      if (s->hdv) check(connect_hdv_output(s, sink.p, cb, u));
      else check(s->builder->RenderStream(nullptr, &MEDIATYPE_Interleaved,
                                          s->sourceRef.p, nullptr, s->device.p));
      trace(cb, u, "Tape output connected; sample allocator ready");
    } else {
      stage = "create sample grabber";
      check(CoCreateInstance(CLSID_Grab, nullptr, CLSCTX_INPROC_SERVER,
                             IID_IBaseFilter, (void **)s->grabFilter.out()));
      check(s->grabFilter->QueryInterface(IID_Grab, (void **)s->grab.out()));
      AM_MEDIA_TYPE mt{};
      mt.majortype = s->hdv ? MEDIATYPE_Stream : MEDIATYPE_Interleaved;
      mt.subtype = s->hdv ? GUID_NULL : MEDIASUBTYPE_dvsd;
      check(s->grab->SetMediaType(&mt));
      s->callback.p = new CaptureCallback(cb, u, pal == 3, s->hdv);
      check(s->grab->SetCallback(s->callback.p, 0));
      check(s->grab->SetOneShot(FALSE));
      check(s->grab->SetBufferSamples(FALSE));
      check(s->graph->AddFilter(s->grabFilter.p, L"DV capture"));
      s->nullFilter.create(CLSID_UDVTNullRenderer);
      check(s->graph->AddFilter(s->nullFilter.p, L"Null renderer"));
      stage = "connect capture pins";
      if (s->hdv) check(connect_hdv(s,cb,u));
      else check(s->builder->RenderStream(nullptr, &MEDIATYPE_Interleaved,
                                     s->device.p, s->grabFilter.p, s->nullFilter.p));
      AM_MEDIA_TYPE connected{};
      if (SUCCEEDED(s->grab->GetConnectedMediaType(&connected))) {
        trace(cb, u, "Negotiated capture media type: " + media_text(connected));
        CoTaskMemFree(connected.pbFormat); if (connected.pUnk) connected.pUnk->Release();
      }
    }
    if (!output) {
      Com<IEnumPins> pins;
      if (SUCCEEDED(s->device->EnumPins(pins.out()))) {
        Com<IPin> pin;
        while (pins->Next(1, pin.out(), nullptr) == S_OK) {
          if (SUCCEEDED(pin->QueryInterface(IID_IAMDroppedFrames,
                                            (void **)s->dropped.out()))) {
            s->dropped->GetNumDropped(&s->previous_dropped);
            break;
          }
        }
      }
    }
    stage = output ? "start tape-output graph" : "start capture graph";
    if (!output) {
      // Tape timestamps can jump or restart at a scene/format boundary. The
      // receiver is already paced by the device; a renderer clock can stall it.
      Com<IMediaFilter> filter;
      check(s->graph->QueryInterface(IID_IMediaFilter, (void **)filter.out()));
      check(filter->SetSyncSource(nullptr));
      trace(cb, u, "Capture graph uses device pacing (no renderer reference clock)");
    }
    check(s->control->Run());
    trace(cb, u, "DirectShow graph run requested successfully");
    return s;
  } catch (HRESULT h) {
    delete s;
    fail(e, cap, h);
    const char *hint = output && h == HRESULT_FROM_WIN32(ERROR_NO_SYSTEM_RESOURCES)
        ? " For tape output, check the deck's recording input (HDV/DV or i.LINK), "
          "match its PAL/50i or NTSC/60i recording system to the source, and close other tape applications."
        : "";
    char detail[2048]; std::snprintf(detail, sizeof(detail), "%s: %s%s", stage, e, hint);
    std::snprintf(e, cap, "%s", detail);
    trace(cb, u, detail);
    return nullptr;
  } catch (const std::exception &exception) {
    delete s;
    std::snprintf(e, cap, "%s", exception.what()); trace(cb, u, exception.what());
    return nullptr;
  } catch (...) {
    delete s;
    fail(e, cap, E_FAIL);
    return nullptr;
  }
}
extern "C" int udvt_poll(void *h, int ms, char *e, int cap) {
  auto *s = (Session *)h;
  MSG msg;
  while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) {
    TranslateMessage(&msg);
    DispatchMessageW(&msg);
  }
  long code;
  LONG_PTR a, b;
  while (s->events->GetEvent(&code, &a, &b, 0) == S_OK) {
    s->events->FreeEventParams(code, a, b);
    if (code == EC_ERRORABORT || code == EC_DEVICE_LOST) {
      fail(e, cap, code == EC_ERRORABORT ? (HRESULT)a : E_FAIL);
      return -1;
    }
  }
  auto now = GetTickCount64();
  if (s->automatic && now - s->checked_device >= 500) {
    s->checked_device = now;
    bool present = false, changed = false, enumerated = true;
    try {
      each_device([&](const std::string &id, const std::string &, IMoniker *m, IBindCtx *ctx) {
        if (id != s->device_id) return false;
        present = true;
        LPOLESTR display = nullptr;
        if (SUCCEEDED(m->GetDisplayName(ctx, nullptr, &display))) {
          changed = utf8(display) != s->endpoint; CoTaskMemFree(display);
        }
        return true;
      });
    } catch (...) { enumerated = false; }
    if (enumerated && (!present || changed)) {
      std::snprintf(e, cap, "%s", changed ? "Selected FireWire deck changed its DV/HDV driver endpoint" : "Selected FireWire deck disconnected or is changing driver endpoints");
      trace(s->callback->cb, s->callback->user, e);
      return -1;
    }
    long mode = 0;
    if (s->transport.p && SUCCEEDED(s->transport->get_Mode(&mode)) && mode == ED_MODE_PLAY) {
      if (!s->playing_since) s->playing_since = now;
      if (now - s->playing_since > 4000 && now - s->callback->last_sample.load() > 4000) {
        std::snprintf(e, cap, "Deck is playing but no samples arrived for four seconds; rebuilding the receiver");
        return -1;
      }
    } else s->playing_since = 0;
  }
  if (s->dropped.p && s->callback.p) {
    long total = 0;
    if (SUCCEEDED(s->dropped->GetNumDropped(&total)) &&
        total >= s->previous_dropped) {
      long delta = total - s->previous_dropped;
      if (delta)
        s->callback->cb(s->callback->user, nullptr, -delta);
      s->previous_dropped = total;
    }
  }
  Sleep(ms);
  return 0;
}
extern "C" int udvt_transport(void *h, int cmd, char *e, int cap) {
  auto *s = (Session *)h;
  if (!s->transport.p) {
    fail(e, cap, E_NOINTERFACE);
    return -1;
  }
  long modes[] = {ED_MODE_STOP,         ED_MODE_PLAY, ED_MODE_FREEZE,
                  ED_MODE_REW,          ED_MODE_FF,   ED_MODE_RECORD,
                  ED_MODE_RECORD_FREEZE};
  if (cmd < 0 || cmd > 6)
    return -1;
  HRESULT hr = s->transport->put_Mode(modes[cmd]);
  // Some MSDV devices return a positive Win32 error instead of an HRESULT.
  // Only S_OK acknowledges the transport command; do not report a raw timeout
  // or command rejection as a successful Record request.
  if (hr != S_OK) {
    fail(e, cap, FAILED(hr) ? hr : HRESULT_FROM_WIN32((DWORD)hr));
    return -1;
  }
  return 0;
}
static void deliver_output(Session *s, const uint8_t *bytes, int len,
                           REFERENCE_TIME start, REFERENCE_TIME end,
                           char *e, int cap) {
    Com<IMediaSample> sample;
    DWORD started = GetTickCount();
    HRESULT buffer_hr;
    while ((buffer_hr = s->source->pin.allocator->GetBuffer(
                sample.out(), nullptr, nullptr, AM_GBF_NOWAIT)) ==
               VFW_E_TIMEOUT &&
           GetTickCount() - started < 5000) {
      if (udvt_poll(s, 5, e, cap) < 0)
        throw std::runtime_error(e);
    }
    check(buffer_hr);
    BYTE *p;
    check(sample->GetPointer(&p));
    if (sample->GetSize() < len)
      throw HRESULT(E_FAIL);
    memcpy(p, bytes, len);
    check(sample->SetActualDataLength(len));
    check(sample->SetTime(&start, &end));
    check(sample->SetSyncPoint(TRUE));
    check(s->source->pin.input->Receive(sample.p));
    s->time = end;
    s->frame_number++;
}
static void deliver_hdv(Session *s, const uint8_t *bytes, uint64_t start,
                        uint64_t end, char *e, int cap) {
  if (!s->frame_number) trace(s->source->pin.callback, s->source->pin.user,
      "HDV output uses PCR-interpolated source-packet timestamps; transport payload unchanged");
  deliver_output(s, bytes, 192 * 256, REFERENCE_TIME(start * 10 / 27),
                 REFERENCE_TIME(end * 10 / 27), e, cap);
  s->ts_packets += 256;
}
extern "C" int udvt_write(void *h, const uint8_t *bytes, int len, char *e,
                          int cap) {
  try {
    auto *s = (Session *)h;
    if (!s->source || (s->hdv ? (len <= 0 || len > 188*256 || len % 188 != 0) : len != (s->pal ? 144000 : 120000)))
      throw HRESULT(E_INVALIDARG);
    if (s->hdv) {
      s->hdv_clock.push(bytes, len, [&](const uint8_t *block, uint64_t start, uint64_t end) {
        deliver_hdv(s, block, start, end, e, cap);
      });
    } else {
      REFERENCE_TIME end = s->pal ? (s->frame_number + 1) * 400000LL :
          (s->frame_number + 1) * 10010000000LL / 30000;
      deliver_output(s, bytes, len, s->time, end, e, cap);
    }
    return 0;
  } catch (HRESULT hr) {
    fail(e, cap, hr);
    return -1;
  } catch (const std::exception &error) {
    std::snprintf(e, cap, "%s", error.what());
    return -1;
  } catch (...) {
    fail(e, cap, E_FAIL);
    return -1;
  }
}
extern "C" int udvt_finish(void *h, char *e, int cap) {
  auto *s = (Session *)h;
  if (!s->source)
    return 0;
  try {
    if (s->hdv) s->hdv_clock.finish([&](const uint8_t *block, uint64_t start, uint64_t end) {
      deliver_hdv(s, block, start, end, e, cap);
    });
  } catch (HRESULT hr) {
    fail(e, cap, hr); return -1;
  } catch (const std::exception &error) {
    std::snprintf(e, cap, "%s", error.what()); return -1;
  }
  HRESULT hr = s->source->pin.peer->EndOfStream();
  if (FAILED(hr)) {
    fail(e, cap, hr);
    return -1;
  }
  long code = 0;
  hr = s->events->WaitForCompletion(5000, &code);
  if (FAILED(hr)) {
    fail(e, cap, hr);
    return -1;
  }
  return 0;
}
extern "C" void udvt_close(void *h) { delete (Session *)h; }
