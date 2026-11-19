/* Sizes and offsets of the libaom types starfold reads and writes, as the C compiler that
 * builds libaom lays them out; lib.rs compares them with the bindings' (check_layout). */
#include <stddef.h>

#include "aom/aom_encoder.h"
#include "aom/aomcx.h"

static const size_t layout[] = {
    sizeof(aom_image_t),
    offsetof(aom_image_t, fmt),
    offsetof(aom_image_t, range),
    offsetof(aom_image_t, w),
    offsetof(aom_image_t, planes),
    offsetof(aom_image_t, stride),
    sizeof(aom_codec_enc_cfg_t),
    offsetof(aom_codec_enc_cfg_t, g_w),
    offsetof(aom_codec_enc_cfg_t, g_timebase),
    offsetof(aom_codec_enc_cfg_t, rc_end_usage),
    offsetof(aom_codec_enc_cfg_t, rc_target_bitrate),
    offsetof(aom_codec_enc_cfg_t, rc_min_quantizer),
    offsetof(aom_codec_enc_cfg_t, kf_mode),
    sizeof(aom_codec_ctx_t),
    sizeof(aom_codec_cx_pkt_t),
    offsetof(aom_codec_cx_pkt_t, data),
    sizeof(aom_svc_params_t),
    sizeof(aom_svc_ref_frame_config_t),
    sizeof(aom_active_map_t),
    sizeof(aom_rational_t),
};

size_t starfold_aom_layout(size_t *out, size_t n) {
  size_t count = sizeof(layout) / sizeof(layout[0]);
  for (size_t i = 0; i < n && i < count; i++) out[i] = layout[i];
  return count;
}
