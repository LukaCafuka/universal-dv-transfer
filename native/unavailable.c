#include "bridge.h"
#include <stdio.h>
static int unavailable(char *error, int n) {
  snprintf(error, n,
           "FireWire is unavailable in this build. Tape hardware requires "
           "macOS 15 or earlier and a build made with a macOS 15 SDK. File "
           "capture and playlist export remain available.");
  return -1;
}
int udvt_list(udvt_device_cb cb, void *user, char *e, int n) {
  (void)cb;
  (void)user;
  return unavailable(e, n);
}
void *udvt_open(const char *id, int output, int pal, uint32_t bitrate, udvt_frame_cb cb,
                void *user, char *e, int n) {
  (void)id;
  (void)output;
  (void)pal;
  (void)bitrate;
  (void)cb;
  (void)user;
  unavailable(e, n);
  return NULL;
}
int udvt_poll(void *h, int ms, char *e, int n) {
  (void)h;
  (void)ms;
  return unavailable(e, n);
}
int udvt_transport(void *h, int cmd, char *e, int n) {
  (void)h;
  (void)cmd;
  return unavailable(e, n);
}
int udvt_write(void *h, const uint8_t *bytes, int len, char *e, int n) {
  (void)h;
  (void)bytes;
  (void)len;
  return unavailable(e, n);
}
int udvt_finish(void *h, char *e, int n) {
  (void)h;
  return unavailable(e, n);
}
void udvt_close(void *h) { (void)h; }
