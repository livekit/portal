// Copyright 2026 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// MJPEG encode through the libjpeg-turbo already linked into libwebrtc.
// MANGLE_JPEG_NAMES maps jpeg_* onto its chromium_jpeg_* symbols, so no second
// copy of libjpeg-turbo is linked. libjpeg reports errors by calling
// error_exit, which must not return; it longjmps back here, and nothing of
// libjpeg's state ever crosses into Rust.

#include <setjmp.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>

#include "jpeglib.h"

struct encode_ctx {
  struct jpeg_compress_struct cinfo;
  struct jpeg_error_mgr err;
  jmp_buf jump;
  unsigned char* buf;
  unsigned long len;
};

static void on_error(j_common_ptr cinfo) {
  struct encode_ctx* ctx = (struct encode_ctx*)cinfo->client_data;
  longjmp(ctx->jump, 1);
}

static void on_message(j_common_ptr cinfo) { (void)cinfo; }

// Encodes packed RGB24 at 4:4:4. Returns 0 and sets *out and *out_len to a
// buffer the caller releases with portal_jpeg_free, or -1 on failure.
int portal_jpeg_encode_rgb(const unsigned char* rgb, unsigned int width, unsigned int height,
                           int quality, unsigned char** out, unsigned long* out_len) {
  // Heap, not stack: anything libjpeg modifies before a longjmp must not be
  // an automatic variable, or its value is indeterminate afterwards.
  struct encode_ctx* ctx = calloc(1, sizeof(*ctx));
  if (ctx == NULL) {
    return -1;
  }
  ctx->cinfo.err = jpeg_std_error(&ctx->err);
  ctx->err.error_exit = on_error;
  ctx->err.output_message = on_message;
  ctx->cinfo.client_data = ctx;
  if (setjmp(ctx->jump)) {
    jpeg_destroy_compress(&ctx->cinfo);
    free(ctx->buf);
    free(ctx);
    return -1;
  }
  jpeg_create_compress(&ctx->cinfo);
  jpeg_mem_dest(&ctx->cinfo, &ctx->buf, &ctx->len);
  ctx->cinfo.image_width = width;
  ctx->cinfo.image_height = height;
  ctx->cinfo.input_components = 3;
  ctx->cinfo.in_color_space = JCS_RGB;
  jpeg_set_defaults(&ctx->cinfo);
  jpeg_set_quality(&ctx->cinfo, quality, TRUE);
  for (int i = 0; i < ctx->cinfo.num_components; i++) {
    ctx->cinfo.comp_info[i].h_samp_factor = 1;
    ctx->cinfo.comp_info[i].v_samp_factor = 1;
  }
  jpeg_start_compress(&ctx->cinfo, TRUE);
  while (ctx->cinfo.next_scanline < ctx->cinfo.image_height) {
    JSAMPROW row = (JSAMPROW)(rgb + (size_t)ctx->cinfo.next_scanline * width * 3);
    jpeg_write_scanlines(&ctx->cinfo, &row, 1);
  }
  jpeg_finish_compress(&ctx->cinfo);
  jpeg_destroy_compress(&ctx->cinfo);
  *out = ctx->buf;
  *out_len = ctx->len;
  free(ctx);
  return 0;
}

void portal_jpeg_free(unsigned char* buf) { free(buf); }
