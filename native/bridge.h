#pragma once
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
/* Callbacks run on native threads. Copy bytes before returning; never block.
 * len==INT32_MIN with non-null data is a NUL-terminated UTF-8 diagnostic message.
 * Negative lengths with null data report receive losses. */
typedef void (*udvt_device_cb)(void *, const char *, const char *, int);
typedef void (*udvt_frame_cb)(void *, const uint8_t *, int);
int udvt_list(udvt_device_cb, void *, char *, int);
/* Mode: 0 DV NTSC, 1 DV PAL, 2 HDV MPEG-2 TS, 3 automatic input only.
 * Automatic callbacks prefix data with one byte: 1 complete DV sample,
 * 2 TS sample (188/192), 3 IEC 61883 CIP packet, 5 unknown sample.
 * Bitrate is TS bits/s for HDV output. Negative lengths report lost units. */
void *udvt_open(const char *, int, int, uint32_t, udvt_frame_cb, void *, char *, int);
int udvt_poll(void *, int, char *, int);
int udvt_transport(void *, int, char *, int);
int udvt_write(void *, const uint8_t *, int, char *, int);
int udvt_finish(void *, char *, int);
void udvt_close(void *);
/* Commands: 0 stop, 1 play, 2 pause, 3 rewind, 4 forward, 5 record, 6
 * record-pause. */
#ifdef __cplusplus
}
#endif
