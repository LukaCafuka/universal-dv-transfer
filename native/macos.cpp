#include "apple/AVCVideoServices.h"
#include "bridge.h"
#include <chrono>
#include <array>
#include <cstdio>
#include <cstring>
#include <deque>
#include <mutex>
#include <thread>
#include <vector>
using namespace AVS;
struct MacSession {
  AVCDeviceController controller;
  AVCDevice *device = nullptr;
  AVCDeviceStream *stream = nullptr;
  udvt_frame_cb callback = nullptr;
  void *user = nullptr;
  std::deque<UInt32> available, ready;
  std::mutex queue_lock;
  bool hdv = false;
  uint32_t hdv_bitrate = 0;
  std::deque<std::array<UInt32,47>> ts_queue;
  std::array<UInt32,47> ts_current{};
  bool output = false, started = false, closing = false;
  ~MacSession() {
    if (device) {
      if (stream) {
        device->StopAVCDeviceStream(stream);
        device->DestroyAVCDeviceStream(stream);
      }
      device->closeDevice();
    }
  }
};
static int error(char *e, int cap, const char *msg) {
  std::snprintf(e, cap, "%s", msg);
  return -1;
}
static void pump(int ms) {
  CFRunLoopRunInMode(kCFRunLoopDefaultMode, ms / 1000.0, true);
}
static IOReturn received(DVFrameReceiveMessage message, DVReceiveFrame *frame,
                         void *u) {
  auto *s = (MacSession *)u;
  if (message == kDVFrameReceivedSuccessfully && frame)
    s->callback(s->user, frame->pFrameData, frame->frameLen);
  else
    s->callback(s->user, nullptr, -1);
  if (frame)
    frame->pDVReceiver->releaseFrame(frame);
  return kIOReturnSuccess;
}
static IOReturn received_hdv(UInt32 count, UInt32 **packets, void *u) {
  auto *s = (MacSession *)u;
  std::vector<uint8_t> bytes;
  bytes.reserve(count * 188);
  for (UInt32 i=0;i<count;++i) {
    const auto *p=(const uint8_t *)packets[i]; bytes.insert(bytes.end(),p,p+188);
  }
  if (!bytes.empty()) s->callback(s->user,bytes.data(),(int)bytes.size());
  return kIOReturnSuccess;
}
static IOReturn received_auto(void *u, UInt32 len, UInt8 *payload,
    UInt32 header, UInt32 timestamp) {
  (void)timestamp;
  auto *s = (MacSession *)u;
  if (!len) return kIOReturnSuccess;
  std::vector<uint8_t> tagged(1 + len);
  tagged[0] = ((header >> 14) & 3) == 1 ? 3 : 5;
  memcpy(tagged.data() + 1, payload, len);
  s->callback(s->user, tagged.data(), (int)tagged.size());
  return kIOReturnSuccess;
}
static void auto_message(UInt32 msg, UInt32 a, UInt32 b, void *u) {
  (void)a; (void)b;
  auto *s = (MacSession *)u;
  if (msg == kUniversalReceiverDCLOverrun || msg == kUniversalReceiverReceivedBadPacket ||
      msg == kFWAVCStreamDCLOverrunAutoRestartFailed)
    s->callback(s->user, nullptr, -1);
}
static IOReturn pull_hdv(UInt32 **packet, bool *discontinuity, void *u) {
  auto *s=(MacSession *)u;
  std::lock_guard<std::mutex> lock(s->queue_lock);
  if (s->ts_queue.empty()) return kIOReturnNoResources;
  s->ts_current=s->ts_queue.front();s->ts_queue.pop_front();
  *packet=s->ts_current.data();*discontinuity=false;
  return kIOReturnSuccess;
}
static IOReturn pull(UInt32 *index, void *u) {
  auto *s = (MacSession *)u;
  std::lock_guard<std::mutex> lock(s->queue_lock);
  if (s->ready.empty())
    return kIOReturnNoResources;
  *index = s->ready.front();
  s->ready.pop_front();
  return kIOReturnSuccess;
}
static IOReturn release(UInt32 index, void *u) {
  auto *s = (MacSession *)u;
  std::lock_guard<std::mutex> lock(s->queue_lock);
  s->available.push_back(index);
  return kIOReturnSuccess;
}
static bool setup(AVCDeviceController &c) {
  return c.SetupDeviceController() == kIOReturnSuccess;
}
extern "C" int udvt_list(udvt_device_cb cb, void *u, char *e, int cap) {
  AVCDeviceController c;
  if (!setup(c))
    return error(e, cap,
                 "Apple's FireWire stack is unavailable. Use macOS 15 or "
                 "earlier and an SDK containing IOKit FireWire headers.");
  pump(100);
  for (CFIndex i = 0; i < CFArrayGetCount(c.avcDeviceArray); i++) {
    auto *d = (AVCDevice *)CFArrayGetValueAtIndex(c.avcDeviceArray, i);
    if (!d->isAttached || !d->hasTapeSubunit)
      continue;
    char id[24];
    std::snprintf(id, sizeof(id), "%016llx", (unsigned long long)d->guid);
    cb(u, id, d->deviceName, d->numInputPlugs > 0);
  }
  return 0;
}
extern "C" void *udvt_open(const char *id, int output, int pal, uint32_t bitrate,
                           udvt_frame_cb cb, void *u, char *e, int cap) {
  auto *s = new MacSession;
  s->callback = cb;
  s->user = u;
  s->output = output != 0;
  s->hdv = pal == 2;
  s->hdv_bitrate=bitrate;
  if (s->hdv && output && !bitrate) { delete s;error(e,cap,"HDV output requires PCR timing");return nullptr; }
  if (!setup(s->controller)) {
    delete s;
    error(e, cap, "Apple FireWire drivers are unavailable");
    return nullptr;
  }
  pump(100);
  unsigned long long guid = 0;
  if (std::sscanf(id, "%llx", &guid) != 1) {
    delete s;
    error(e, cap, "Invalid FireWire GUID");
    return nullptr;
  }
  s->device = s->controller.findDeviceByGuid(guid);
  if (!s->device || !s->device->isAttached) {
    s->device = nullptr;
    delete s;
    error(e, cap, "FireWire tape device not found");
    return nullptr;
  }
  if (s->device->openDevice() != kIOReturnSuccess) {
    s->device = nullptr;
    delete s;
    error(e, cap,
          "Cannot open tape device; check VTR mode and other capture "
          "applications");
    return nullptr;
  }
  if (pal == 3 && !output) {
    s->stream = s->device->CreateUniversalReceiverForDevicePlug(0, received_auto, s, auto_message, s);
  } else if (s->hdv) {
    s->stream=output ? s->device->CreateMPEGTransmitterForDevicePlug(0,pull_hdv,s)
                     : s->device->CreateMPEGReceiverForDevicePlug(0,received_hdv,s);
  } else if (output) {
    s->stream = s->device->CreateDVTransmitterForDevicePlug(
        0, pull, s, release, s, nullptr, nullptr, nullptr,
        kCyclesPerDVTransmitSegment, kNumDVTransmitSegments, pal ? 0x80 : 0x00,
        16);
    if (s->stream) {
      for (UInt32 i = 0; i < s->stream->pDVTransmitter->getNumFrames(); i++)
        s->available.push_back(i);
    }
  } else {
    s->stream = s->device->CreateDVReceiverForDevicePlug(
        0, received, s, nullptr, nullptr, nullptr, kCyclesPerDVReceiveSegment,
        kNumDVReceiveSegments, s->device->dvMode);
  }
  if (!s->stream) {
    delete s;
    error(e, cap,
          "Cannot create the FireWire stream; match the app's DV/HDV selection to the deck");
    return nullptr;
  }
  if (!output) {
    if (s->device->StartAVCDeviceStream(s->stream) != kIOReturnSuccess) {
      delete s;
      error(e, cap, "Cannot start DV reception");
      return nullptr;
    }
    s->started = true;
  }
  return s;
}
extern "C" int udvt_poll(void *h, int ms, char *e, int cap) {
  auto *s = (MacSession *)h;
  pump(ms);
  if (!s->device->isAttached)
    return error(e, cap, "FireWire tape device disconnected");
  return 0;
}
extern "C" int udvt_transport(void *h, int cmd, char *e, int cap) {
  auto *s = (MacSession *)h;
  UInt8 command[4] = {0x00, 0x20, 0xc4, 0x60};
  switch (cmd) {
  case 0:
    command[2] = 0xc4;
    command[3] = 0x60;
    break;
  case 1:
    command[2] = 0xc3;
    command[3] = 0x75;
    break;
  case 2:
    command[2] = 0xc3;
    command[3] = 0x7d;
    break;
  case 3:
    command[2] = 0xc4;
    command[3] = 0x65;
    break;
  case 4:
    command[2] = 0xc4;
    command[3] = 0x75;
    break;
  case 5:
    command[2] = 0xc2;
    command[3] = 0x75;
    break;
  case 6:
    command[2] = 0xc2;
    command[3] = 0x7d;
    break;
  default:
    return error(e, cap, "Unknown deck command");
  }
  UInt8 response[512];
  UInt32 n = sizeof(response);
  IOReturn result =
      s->device->AVCCommand(command, sizeof(command), response, &n);
  if (result != kIOReturnSuccess || n < 1 ||
      (response[0] != 0x09 && response[0] != 0x0f))
    return error(e, cap, "Deck rejected the AV/C transport command");
  return 0;
}
extern "C" int udvt_write(void *h, const uint8_t *bytes, int len, char *e,
                          int cap) {
  auto *s = (MacSession *)h;
  if (!s->output)
    return error(e, cap, "Not an output session");
  if (s->hdv) {
    if (len <= 0 || len % 188) return error(e,cap,"HDV output needs aligned TS packets");
    for (int offset=0;offset<len;offset+=188) {
      auto began=std::chrono::steady_clock::now();
      for (;;) {
        bool added=false;
        {
          std::lock_guard<std::mutex> lock(s->queue_lock);
          if (s->ts_queue.size()<8192) { std::array<UInt32,47> packet; std::memcpy(packet.data(),bytes+offset,188);s->ts_queue.push_back(packet);added=true; }
        }
        if (added) break;
        if (udvt_poll(s,5,e,cap)<0) return -1;
        if (std::chrono::steady_clock::now()-began>std::chrono::seconds(5)) return error(e,cap,"HDV output stalled");
      }
    }
    bool start_now=false;
    { std::lock_guard<std::mutex> lock(s->queue_lock);start_now=!s->started && s->ts_queue.size()>=2048; }
    if (start_now) { if (s->device->StartAVCDeviceStream(s->stream)!=kIOReturnSuccess) return error(e,cap,"Cannot start HDV output");s->started=true; }
    return 0;
  }
  auto start = std::chrono::steady_clock::now();
  UInt32 index = 0;
  bool start_now = false;
  for (;;) {
    {
      std::lock_guard<std::mutex> lock(s->queue_lock);
      if (!s->available.empty()) {
        index = s->available.front();
        s->available.pop_front();
        break;
      }
    }
    if (udvt_poll(s, 5, e, cap) < 0)
      return -1;
    if (std::chrono::steady_clock::now() - start > std::chrono::seconds(5))
      return error(e, cap, "FireWire output stalled");
    std::this_thread::sleep_for(std::chrono::milliseconds(1));
  }
  auto *frame = s->stream->pDVTransmitter->getFrame(index);
  if ((int)frame->frameLen != len)
    return error(e, cap, "DV output frame format changed");
  std::memcpy(frame->pFrameData, bytes, len);
  {
    std::lock_guard<std::mutex> lock(s->queue_lock);
    s->ready.push_back(index);
    start_now = !s->started && s->ready.size() >= 8;
  }
  if (start_now) {
    if (s->device->StartAVCDeviceStream(s->stream) != kIOReturnSuccess)
      return error(e, cap, "DV transmission failed to start");
    s->started = true;
  }
  return 0;
}
extern "C" int udvt_finish(void *h, char *e, int cap) {
  auto *s = (MacSession *)h;
  if (!s->output)
    return 0;
  if (!s->started) {
    if (s->device->StartAVCDeviceStream(s->stream) != kIOReturnSuccess)
      return error(e, cap, "Cannot start DV output");
    s->started = true;
  }
  if (s->hdv) {
    auto began=std::chrono::steady_clock::now();
    for (;;) {
      bool empty;
      { std::lock_guard<std::mutex> lock(s->queue_lock);empty=s->ts_queue.empty(); }
      if (empty) break;
      if (udvt_poll(s,5,e,cap)<0) return -1;
      if (std::chrono::steady_clock::now()-began>std::chrono::seconds(5)) return error(e,cap,"HDV output did not drain");
    }
    // The SDK prefetches TS packets; allow its queued isoch segments to finish.
    unsigned drain_ms=(kNumTransmitSegments*kCyclesPerTransmitSegment+7)/8
        + (unsigned)(uint64_t(kTSPacketQueueSizeInPackets)*188*8*1000/s->hdv_bitrate)+50;
    auto deadline=std::chrono::steady_clock::now()+std::chrono::milliseconds(drain_ms);
    while (std::chrono::steady_clock::now()<deadline) if (udvt_poll(s,5,e,cap)<0) return -1;
    return 0;
  }
  auto start = std::chrono::steady_clock::now();
  for (;;) {
    {
      std::lock_guard<std::mutex> lock(s->queue_lock);
      if (s->ready.empty() &&
          s->available.size() == s->stream->pDVTransmitter->getNumFrames())
        break;
    }
    if (udvt_poll(s, 5, e, cap) < 0)
      return -1;
    if (std::chrono::steady_clock::now() - start > std::chrono::seconds(5))
      return error(e, cap, "DV output did not drain");
    std::this_thread::sleep_for(std::chrono::milliseconds(1));
  }
  return 0;
}
extern "C" void udvt_close(void *h) { delete (MacSession *)h; }
