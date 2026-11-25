#pragma once
#include <array>
#include <cstdint>
#include <cstring>
#include <deque>
#include <stdexcept>

// Reconstruct IEC 61883-4 source-packet timing between adjacent PCRs. A single
// bitrate estimate cannot reproduce a camera's varying transport-packet spacing.
// Only the four-byte FireWire prefixes are generated; TS payloads stay untouched.
class HdvOutputClock {
  static constexpr uint64_t pcr_wrap = (1ULL << 33) * 300;
  struct Packet { std::array<uint8_t, 188> bytes; uint64_t index; };
  std::deque<Packet> pending;
  std::array<uint8_t, 192 * 256> block{};
  uint64_t count = 0, epoch_index = 0, epoch_time = 0;
  uint64_t anchor_index = 0, anchor_raw = 0, anchor_time = 0;
  uint64_t interval = 0, span = 0, block_start = 0, block_end = 0;
  unsigned used = 0;
  int clock_pid = -1;
  bool anchored = false, timed = false, finished = false;

  static uint64_t ticks1394(uint64_t time) {
    return time / 1125 * 1024 + time % 1125 * 1024 / 1125;
  }
  uint64_t predict(uint64_t index) const {
    return anchor_time + (index - anchor_index) * interval / span;
  }
  template<class Emit> void append(const Packet &packet, uint64_t time,
                                  uint64_t end, Emit &emit) {
    const uint64_t ticks = ticks1394(time);
    const uint32_t sph = uint32_t((ticks / 3072 % 8000) << 12) | uint32_t(ticks % 3072);
    auto *p = block.data() + used * 192;
    p[0] = uint8_t(sph >> 24); p[1] = uint8_t(sph >> 16);
    p[2] = uint8_t(sph >> 8); p[3] = uint8_t(sph);
    std::memcpy(p + 4, packet.bytes.data(), 188);
    if (!used) block_start = time;
    block_end = end;
    if (++used == 256) {
      emit(block.data(), block_start, block_end);
      used = 0;
    }
  }
  template<class Emit> void extrapolate(Emit &emit) {
    while (!pending.empty()) {
      const auto &p = pending.front();
      append(p, predict(p.index), predict(p.index + 1), emit);
      pending.pop_front();
    }
  }
public:
  // Emit receives exactly 256 source packets and stream-relative 27-MHz times.
  template<class Emit> void push(const uint8_t *bytes, int length, Emit emit) {
    if (finished || length <= 0 || length % 188) throw std::runtime_error("Invalid HDV output packet block");
    for (int at = 0; at < length; at += 188) {
      const auto *p = bytes + at;
      if (p[0] != 0x47 || (p[1] & 0x80)) throw std::runtime_error("Corrupt HDV output packet");
      const int pid = ((p[1] & 31) << 8) | p[2];
      const bool adaptation = (p[3] & 0x20) && p[4] > 0;
      if (adaptation && p[4] > 183) throw std::runtime_error("Invalid HDV adaptation field");
      // Playlist boundaries carry discontinuity indicators. Keep the outgoing
      // clock continuous, then learn the new clip's own PCR origin and spacing.
      if (adaptation && (p[5] & 0x80) &&
          (pid == clock_pid || (p[4] >= 7 && (p[5] & 0x10)))) {
        if (timed) {
          extrapolate(emit);
          epoch_time = predict(count);
        } else if (anchored && !pending.empty()) {
          throw std::runtime_error("HDV clock reset before two PCR markers");
        }
        if (timed) epoch_index = count;
        anchored = timed = false;
        clock_pid = pid;
      }
      Packet packet{};
      std::memcpy(packet.bytes.data(), p, 188); packet.index = count++;
      pending.push_back(packet);
      if (pending.size() > 32768) throw std::runtime_error("HDV output has no usable PCR clock markers within the bounded timing buffer");
      if (!adaptation || p[4] < 7 || !(p[5] & 0x10)) continue;
      if (clock_pid < 0) clock_pid = pid;
      if (clock_pid != pid) continue;
      const uint64_t base = (uint64_t(p[6]) << 25) | (uint64_t(p[7]) << 17) |
          (uint64_t(p[8]) << 9) | (uint64_t(p[9]) << 1) | (p[10] >> 7);
      const uint64_t extension = ((p[10] & 1) << 8) | p[11];
      if (extension >= 300) throw std::runtime_error("Invalid HDV PCR extension");
      const uint64_t raw = base * 300 + extension;
      if (!anchored) {
        anchor_index = packet.index; anchor_raw = raw; anchored = true;
        continue;
      }
      const uint64_t delta = (raw + pcr_wrap - anchor_raw) % pcr_wrap;
      const uint64_t packets = packet.index - anchor_index;
      // A playlist's adaptation-only discontinuity marker can repeat the first
      // PCR from the new file. It is a repeated observation, not a time interval.
      if (!delta) {
        if (!timed) anchor_index = packet.index;
        continue;
      }
      if (delta > 27000000 || !packets ||
          packets * 188 * 8 * 27000000ULL / delta > 45000000)
        throw std::runtime_error("Invalid HDV PCR interval; a clock reset requires a discontinuity indicator");
      const bool first = !timed;
      while (!pending.empty() && pending.front().index < packet.index) {
        const auto &q = pending.front();
        const uint64_t t = first ? epoch_time + (q.index - epoch_index) * delta / packets :
                                  anchor_time + (q.index - anchor_index) * delta / packets;
        const uint64_t end = first ? epoch_time + (q.index + 1 - epoch_index) * delta / packets :
                                    anchor_time + (q.index + 1 - anchor_index) * delta / packets;
        append(q, t, end, emit);
        pending.pop_front();
      }
      anchor_time = first ? epoch_time + (packet.index - epoch_index) * delta / packets : anchor_time + delta;
      anchor_index = packet.index; anchor_raw = raw;
      interval = delta; span = packets; timed = true;
    }
  }
  template<class Emit> void finish(Emit emit) {
    if (finished) return;
    if (!timed && !pending.empty()) throw std::runtime_error("HDV output ended before two usable PCR markers");
    extrapolate(emit);
    // The driver needs a complete 256-packet sample. Pad only the final sample,
    // never short write calls or the boundary between two playlist clips.
    Packet null_packet{}; null_packet.bytes.fill(0xff);
    null_packet.bytes[0] = 0x47; null_packet.bytes[1] = 0x1f;
    null_packet.bytes[2] = 0xff; null_packet.bytes[3] = 0x10;
    while (used) {
      null_packet.index = count++;
      append(null_packet, predict(null_packet.index), predict(null_packet.index + 1), emit);
    }
    finished = true;
  }
};
