#include "bridge.h"
#include <libavc1394/avc1394.h>
#include <libavc1394/avc1394_vcr.h>
#include <libavc1394/rom1394.h>
#include <libiec61883/iec61883.h>
#include <libraw1394/raw1394.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#define Q 16
struct session {
  raw1394handle_t raw, control;
  iec61883_dv_fb_t rx;
  iec61883_dv_t tx;
  iec61883_mpeg2_t mpeg;
  int hdv, hdv_rx_len, draining, automatic;
  unsigned hdv_cycle, hdv_dbc;
  uint32_t bitrate;
  uint64_t clock_remainder;
  nodeid_t local, remote;
  int output, oplug, iplug, channel, bandwidth, started, reset, underflow;
  udvt_frame_cb cb;
  void *user;
  unsigned char frames[Q][144000];
  int length[Q], head, count, offset, finished, last_len, repeat_offset;
  unsigned char last[144000];
};
static _Thread_local struct session *active_session;
static int fail(char *e, int n, const char *msg) {
  snprintf(e, n, "%s", msg);
  return -1;
}
static int bus_reset(raw1394handle_t raw, unsigned int gen) {
  struct session *s = active_session;
  raw1394_update_generation(raw, gen);
  if (s && (s->raw == raw || s->control == raw))
    s->reset = 1;
  return 0;
}
static int scan(const char *wanted, int *found_port, nodeid_t *found_node,
                udvt_device_cb cb, void *user) {
  raw1394handle_t raw = raw1394_new_handle();
  if (!raw)
    return -1;
  int ports = raw1394_get_port_info(raw, NULL, 0);
  raw1394_destroy_handle(raw);
  for (int p = 0; p < ports; p++) {
    raw = raw1394_new_handle_on_port(p);
    if (!raw)
      continue;
    nodeid_t local = raw1394_get_local_id(raw);
    int count = raw1394_get_nodecount(raw);
    for (int n = 0; n < count; n++) {
      nodeid_t node = 0xffc0 | n;
      if (node == local)
        continue;
      rom1394_directory dir;
      memset(&dir, 0, sizeof(dir));
      if (rom1394_get_directory(raw, node, &dir) >= 0 &&
          rom1394_get_node_type(&dir) == ROM1394_NODE_TYPE_AVC &&
          avc1394_check_subunit_type(raw, node, AVC1394_SUBUNIT_TYPE_VCR)) {
        char id[32], name[256];
        snprintf(id, sizeof(id), "%016llx",
                 (unsigned long long)rom1394_get_guid(raw, node));
        snprintf(name, sizeof(name), "%s %s",
                 dir.nr_textual_leafs > 0 ? dir.textual_leafs[0] : "FireWire",
                 dir.nr_textual_leafs > 1 ? dir.textual_leafs[1] : "DV deck");
        if (cb)
          cb(user, id, name, 1);
        if (wanted && strcmp(wanted, id) == 0) {
          *found_port = p;
          *found_node = node;
          rom1394_free_directory(&dir);
          raw1394_destroy_handle(raw);
          return 1;
        }
      }
      rom1394_free_directory(&dir);
    }
    raw1394_destroy_handle(raw);
  }
  return 0;
}
int udvt_list(udvt_device_cb cb, void *u, char *e, int cap) {
  int p;
  nodeid_t n;
  if (scan(NULL, &p, &n, cb, u) < 0)
    return fail(e, cap,
                "Cannot access FireWire. Check firewire_ohci and read/write "
                "access to /dev/fw*.");
  return 0;
}
static int receive(unsigned char *bytes, int len, int complete, void *u) {
  struct session *s = u;
  s->cb(s->user, complete ? bytes : NULL, complete ? len : -1);
  return 0;
}
static int transmit(unsigned char *bytes, int blocks, unsigned int dropped,
                    void *u) {
  struct session *s = u;
  int wanted = blocks * 480;
  if (dropped)
    s->underflow = 1;
  while (wanted > 0) {
    if (!s->count || s->repeat_offset) {
      if (!s->last_len)
        return -1;
      int n = s->last_len - s->repeat_offset;
      if (n > wanted)
        n = wanted;
      memcpy(bytes, s->last + s->repeat_offset, n);
      s->repeat_offset = (s->repeat_offset + n) % s->last_len;
      bytes += n;
      wanted -= n;
      continue;
    }
    int n = s->length[s->head] - s->offset;
    if (n > wanted)
      n = wanted;
    memcpy(bytes, s->frames[s->head] + s->offset, n);
    s->offset += n;
    bytes += n;
    wanted -= n;
    if (s->offset == s->length[s->head]) {
      memcpy(s->last, s->frames[s->head], s->length[s->head]);
      s->last_len = s->length[s->head];
      s->head = (s->head + 1) % Q;
      s->count--;
      s->offset = 0;
    }
  }
  return 0;
}
static int receive_hdv(unsigned char *bytes, int len, unsigned int dropped, void *u) {
  struct session *s = u;
  if (dropped) s->cb(s->user, NULL, -(int)dropped);
  if (len != 188) return -1;
  if (s->hdv_rx_len + len > 188 * 64) {
    s->cb(s->user, s->frames[0], s->hdv_rx_len); s->hdv_rx_len = 0;
  }
  memcpy(s->frames[0] + s->hdv_rx_len, bytes, len); s->hdv_rx_len += len;
  return 0;
}
static enum raw1394_iso_disposition receive_auto(raw1394handle_t raw,
    unsigned char *bytes, unsigned int len, unsigned char channel,
    unsigned char tag, unsigned char sy, unsigned int cycle, unsigned int dropped) {
  struct session *s = raw1394_get_userdata(raw);
  (void)channel; (void)sy; (void)cycle;
  if (dropped) s->cb(s->user, NULL, -(int)dropped);
  if (!len) return RAW1394_ISO_OK;
  if (len > 8192) return RAW1394_ISO_ERROR;
  unsigned char tagged[8193]; tagged[0] = tag == 1 ? 3 : 5;
  memcpy(tagged + 1, bytes, len);
  s->cb(s->user, tagged, (int)len + 1);
  return RAW1394_ISO_OK;
}
/* IEC 61883-4 packetization, paced by the measured TS bitrate rather than fps.
 * Using rawiso avoids libiec61883's transmitter discarding PAT/PMT before its
 * first PCR and blocking in a synchronous PCR lookahead callback. */
static enum raw1394_iso_disposition transmit_hdv(raw1394handle_t raw,
    unsigned char *bytes, unsigned int *len, unsigned char *tag,
    unsigned char *sy, int cycle, unsigned int dropped) {
  struct session *s = raw1394_get_userdata(raw);
  if (dropped) s->underflow = 1;
  s->clock_remainder += s->bitrate;
  unsigned packets = s->clock_remainder / (188ULL * 8 * 8000);
  s->clock_remainder %= (188ULL * 8 * 8000);
  if (packets > 4) return RAW1394_ISO_ERROR;
  bytes[0] = s->local & 63; bytes[1] = 6; bytes[2] = 0xc4;
  bytes[3] = s->hdv_dbc; bytes[4] = 0xa0; bytes[5] = 0x80; bytes[6] = bytes[7] = 0;
  unsigned current = cycle >= 0 ? (unsigned)cycle : s->hdv_cycle;
  s->hdv_cycle = (current + 1) % 8000;
  for (unsigned i = 0; i < packets; ++i) {
    unsigned char *packet = bytes + 8 + i * 192;
    unsigned timestamp = (((current + 7) % 8000) << 12) | (i * 3072 / packets);
    packet[0] = timestamp >> 24; packet[1] = timestamp >> 16;
    packet[2] = timestamp >> 8; packet[3] = timestamp;
    if (s->count) {
      memcpy(packet + 4, s->frames[s->head] + s->offset, 188); s->offset += 188;
      if (s->offset == s->length[s->head]) { s->offset = 0; s->head = (s->head + 1) % Q; --s->count; }
    } else {
      memset(packet + 4, 0xff, 188); packet[4] = 0x47;
      packet[5] = 0x1f; packet[6] = 0xff; packet[7] = 0x10;
      if (!s->draining) s->underflow = 1;
    }
  }
  s->hdv_dbc = (s->hdv_dbc + packets * 8) & 255;
  *len = 8 + packets * 192; *tag = 1; *sy = 0; /* IEEE 1394 tag 1: CIP header */
  return RAW1394_ISO_OK;
}
static int start_hdv(struct session *s) {
  uint32_t timer = 0; uint64_t local_time = 0;
  if (raw1394_read_cycle_timer(s->raw, &timer, &local_time) < 0) return -1;
  s->hdv_cycle = (((timer >> 12) & 8191) + 128) % 8000;
  raw1394_set_userdata(s->raw, s);
  if (raw1394_iso_xmit_init(s->raw, transmit_hdv, 1000, 8 + 192 * 4,
      s->channel, RAW1394_ISO_SPEED_200, 64) < 0) return -1;
  return raw1394_iso_xmit_start(s->raw, s->hdv_cycle, 64);
}
void *udvt_open(const char *id, int output, int pal, uint32_t bitrate, udvt_frame_cb cb, void *u,
                char *e, int cap) {
  int port = -1;
  nodeid_t remote = 0;
  if (scan(id, &port, &remote, NULL, NULL) != 1) {
    fail(e, cap,
         "FireWire device is unavailable (check permissions and VTR mode)");
    return NULL;
  }
  struct session *s = calloc(1, sizeof(*s));
  if (!s) {
    fail(e, cap, "Out of memory");
    return NULL;
  }
  s->channel = -1;
  s->raw = raw1394_new_handle_on_port(port);
  if (!s->raw) {
    free(s);
    return NULL;
  }
  /* AV/C transactions must not share libiec61883's streaming userdata. */
  s->control = raw1394_new_handle_on_port(port);
  if (!s->control) {
    fail(e, cap, "Cannot open a FireWire transport-control handle");
    udvt_close(s);
    return NULL;
  }
  s->output = output;
  s->hdv = pal == 2;
  s->automatic = pal == 3;
  s->bitrate = bitrate;
  if (s->hdv && output && !bitrate) { fail(e, cap, "HDV output needs the source transport bitrate"); udvt_close(s); return NULL; }
  s->remote = remote;
  s->local = raw1394_get_local_id(s->raw);
  s->oplug = -1;
  s->iplug = -1;
  s->bandwidth = 1;
  s->cb = cb;
  s->user = u;
  active_session = s;
  raw1394_set_bus_reset_handler(s->raw, bus_reset);
  raw1394_set_bus_reset_handler(s->control, bus_reset);
  s->channel = iec61883_cmp_connect(s->raw, output ? s->local : remote,
                                    &s->oplug, output ? remote : s->local,
                                    &s->iplug, &s->bandwidth);
  if (s->channel < 0) {
    fail(e, cap, "Could not establish a FireWire CMP connection");
    udvt_close(s);
    return NULL;
  }
  if (s->automatic) {
    if (output) { fail(e, cap, "Automatic format is input only"); udvt_close(s); return NULL; }
    raw1394_set_userdata(s->raw, s);
    if (raw1394_iso_recv_init(s->raw, receive_auto, 1000, 8192, s->channel,
        RAW1394_DMA_DEFAULT, 64) < 0 || raw1394_iso_recv_start(s->raw, -1, -1, 0) < 0) {
      fail(e, cap, "Cannot start universal DV/HDV reception"); udvt_close(s); return NULL;
    }
    s->started = 1;
  } else if (s->hdv) {
    if (!output) {
      s->mpeg = iec61883_mpeg2_recv_init(s->raw, receive_hdv, s);
      if (!s->mpeg || iec61883_mpeg2_recv_start(s->mpeg, s->channel) < 0) {
        fail(e, cap, "Cannot start HDV reception; set the deck's FireWire output to HDV"); udvt_close(s); return NULL;
      }
    }
  } else if (output) {
    s->tx = iec61883_dv_xmit_init(s->raw, pal, transmit, s);
    if (!s->tx) {
      fail(e, cap, "Could not initialize DV output");
      udvt_close(s);
      return NULL;
    }
  } else {
    s->rx = iec61883_dv_fb_init(s->raw, receive, s);
    if (!s->rx || iec61883_dv_fb_start(s->rx, s->channel) < 0) {
      fail(e, cap, "Could not start DV reception");
      udvt_close(s);
      return NULL;
    }
  }
  return s;
}
int udvt_poll(void *h, int ms, char *e, int cap) {
  struct session *s = h;
  active_session = s;
  if (s->reset)
    return fail(e, cap,
                "FireWire bus reset; transfer stopped. Reconnect the device "
                "and start a new transfer.");
  struct pollfd p = {raw1394_get_fd(s->raw), POLLIN, 0};
  int n = poll(&p, 1, ms);
  if (n < 0)
    return fail(e, cap, "FireWire polling failed");
  if (n > 0 && (p.revents & (POLLERR | POLLHUP | POLLNVAL)))
    return fail(e, cap, "FireWire device disconnected");
  if (n > 0 && p.revents & POLLIN && raw1394_loop_iterate(s->raw) < 0)
    return fail(e, cap, "FireWire streaming failed");
  if (s->hdv_rx_len) { s->cb(s->user, s->frames[0], s->hdv_rx_len); s->hdv_rx_len=0; }
  if (s->hdv && s->underflow) return fail(e, cap, "FireWire dropped HDV output packets");
  return 0;
}
int udvt_transport(void *h, int cmd, char *e, int cap) {
  struct session *s = h;
  active_session = s;
  if (s->reset)
    return fail(e, cap, "FireWire bus was reset");
  switch (cmd) {
  case 0:
    avc1394_vcr_stop(s->control, s->remote);
    break;
  case 1:
    avc1394_vcr_play(s->control, s->remote);
    break;
  case 2:
    avc1394_vcr_pause(s->control, s->remote);
    break;
  case 3:
    avc1394_vcr_rewind(s->control, s->remote);
    break;
  case 4:
    avc1394_vcr_forward(s->control, s->remote);
    break;
  case 5:
    avc1394_vcr_record(s->control, s->remote);
    break;
  case 6:
    avc1394_vcr_pause(s->control, s->remote);
    break;
  default:
    return fail(e, cap, "Unknown transport command");
  }
  return 0;
}
int udvt_write(void *h, const uint8_t *bytes, int len, char *e, int cap) {
  struct session *s = h;
  if ((!s->tx && !(s->hdv && s->output)) || len <= 0 || len > 144000 || (s->hdv && len % 188))
    return fail(e, cap, "Invalid DV output frame");
  time_t start = time(NULL);
  while (s->count == Q) {
    if (udvt_poll(s, 10, e, cap) < 0)
      return -1;
    if (time(NULL) - start > 5)
      return fail(e, cap, "FireWire output stalled");
  }
  int tail = (s->head + s->count) % Q;
  memcpy(s->frames[tail], bytes, len);
  s->length[tail] = len;
  s->count++;
  if (!s->started && s->count >= 8) {
    if ((s->hdv ? start_hdv(s) : iec61883_dv_xmit_start(s->tx, s->channel)) < 0)
      return fail(e, cap, "DV transmission failed to start");
    s->started = 1;
  }
  return 0;
}
int udvt_finish(void *h, char *e, int cap) {
  struct session *s = h;
  if ((!s->tx && !(s->hdv && s->output)) || s->finished)
    return 0;
  active_session = s;
  if (s->reset)
    return fail(e, cap, "FireWire bus reset while draining output");
  s->draining = 1;
  if (!s->started && s->count) {
    if ((s->hdv ? start_hdv(s) : iec61883_dv_xmit_start(s->tx, s->channel)) < 0)
      return fail(e, cap, "DV transmission failed to start");
    s->started = 1;
  }
  time_t start = time(NULL);
  while (s->count) {
    if (udvt_poll(s, 5, e, cap) < 0)
      return -1;
    if (time(NULL) - start > 5)
      return fail(e, cap, "DV output did not drain");
  }
  if (s->started && raw1394_iso_xmit_sync(s->raw) < 0)
    return fail(e, cap, "DV output flush failed");
  s->finished = 1;
  return 0;
}
void udvt_close(void *h) {
  struct session *s = h;
  if (!s)
    return;
  active_session = s;
  if (s->tx) {
    iec61883_dv_close(s->tx);
  }
  if (s->rx) {
    iec61883_dv_fb_close(s->rx);
  }
  if (s->mpeg) iec61883_mpeg2_close(s->mpeg);
  if (s->automatic && s->started) raw1394_iso_shutdown(s->raw);
  if (s->hdv && s->output && s->started) raw1394_iso_shutdown(s->raw);
  if (s->raw) {
    if (s->channel >= 0 && !s->reset)
      iec61883_cmp_disconnect(s->raw, s->output ? s->local : s->remote,
                              s->oplug, s->output ? s->remote : s->local,
                              s->iplug, s->channel, s->bandwidth);
    raw1394_destroy_handle(s->raw);
  }
  if (s->control)
    raw1394_destroy_handle(s->control);
  active_session = NULL;
  free(s);
}
