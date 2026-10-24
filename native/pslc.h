// SPDX-License-Identifier: AGPL-3.0-only
// PS5 LAN Capture native core: a thin C facade over chiaki-ng's protocol library
// plus an FFmpeg D3D11VA video decoder. All functions are safe to call from any thread
// unless stated otherwise.
#ifndef PSLC_H
#define PSLC_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#ifdef PSLC_BUILD
#define PSLC_API __declspec(dllexport)
#else
#define PSLC_API __declspec(dllimport)
#endif

#define PSLC_LOG_ERROR 1
#define PSLC_LOG_WARNING 2
#define PSLC_LOG_INFO 3
#define PSLC_LOG_DEBUG 4

typedef void (*PslcLogCb)(int level, const char *msg, void *user);

PSLC_API int pslc_init(PslcLogCb log_cb, void *user);
PSLC_API const char *pslc_version(void);

typedef struct PslcHost
{
	char addr[64];
	char name[64];
	char host_id[32];
	char host_type[32];
	char system_version[32];
	char running_app[64];
	int state; // 0 unknown, 1 ready, 2 standby
	int is_ps5;
	int target;
} PslcHost;

typedef struct PslcDiscovery PslcDiscovery;

// unicast_host == NULL: broadcast on every IPv4 interface.
PSLC_API PslcDiscovery *pslc_discovery_start(const char *unicast_host);
PSLC_API int pslc_discovery_hosts(PslcDiscovery *discovery, PslcHost *out, int max);
PSLC_API void pslc_discovery_stop(PslcDiscovery *discovery);

PSLC_API int pslc_wakeup(const char *host, const char *regist_key, int ps5);

typedef struct PslcRegistResult
{
	int status; // 0 pending, 1 success, 2 failed, 3 canceled
	int target;
	char nickname[32];
	char regist_key[16];
	uint8_t rp_key[16];
	uint32_t rp_key_type;
	uint8_t server_mac[6];
	char error[256];
} PslcRegistResult;

typedef struct PslcRegist PslcRegist;

PSLC_API PslcRegist *pslc_regist_start(const char *host, int broadcast, int target, const uint8_t account_id[8], uint32_t pin, uint32_t console_pin);
PSLC_API int pslc_regist_poll(PslcRegist *regist, PslcRegistResult *out);
PSLC_API void pslc_regist_free(PslcRegist *regist);

#define PSLC_CODEC_H264 0
#define PSLC_CODEC_H265 1

typedef struct PslcSessionConfig
{
	const char *host;
	int ps5;
	char regist_key[16];
	uint8_t morning[16];
	uint32_t width;
	uint32_t height;
	uint32_t fps;
	uint32_t bitrate;
	int codec;
	int prefer_hw_decode;
	void *d3d11_device; // ID3D11Device*, created with D3D11_CREATE_DEVICE_VIDEO_SUPPORT
	double packet_loss_max;
} PslcSessionConfig;

#define PSLC_EVENT_CONNECTED 1
#define PSLC_EVENT_QUIT 2
#define PSLC_EVENT_LOGIN_PIN_REQUEST 3
#define PSLC_EVENT_NICKNAME 4
#define PSLC_EVENT_DECODER 5

// For PSLC_EVENT_QUIT, code is the ChiakiQuitReason and is_error says whether it was abnormal.
typedef void (*PslcEventCb)(int type, int code, int is_error, const char *msg, void *user);
// Interleaved signed 16-bit PCM; frames counts samples per channel.
typedef void (*PslcAudioCb)(const int16_t *pcm, size_t frames, uint32_t channels, uint32_t rate, void *user);

#define PSLC_FEEDBACK_RUMBLE 1          // data: [left (low freq), right (high freq)]
#define PSLC_FEEDBACK_TRIGGER_EFFECTS 2 // data: [type_left, left[10], type_right, right[10]]
#define PSLC_FEEDBACK_LED 3             // data: [r, g, b]
#define PSLC_FEEDBACK_PLAYER_INDEX 4    // data: [index]
#define PSLC_FEEDBACK_HAPTIC_RUMBLE 5   // data: [left, right], derived from the DualSense haptics stream
#define PSLC_FEEDBACK_MOTION_RESET 6

// Controller feedback from the console (rumble, adaptive triggers, lightbar...). Called on network threads.
typedef void (*PslcFeedbackCb)(int kind, const uint8_t *data, size_t size, void *user);

typedef struct PslcSession PslcSession;

PSLC_API PslcSession *pslc_session_start(const PslcSessionConfig *config, PslcEventCb event_cb, PslcAudioCb audio_cb, PslcFeedbackCb feedback_cb, void *user, char *err, size_t err_size);

typedef struct PslcControllerInput
{
	uint32_t buttons; // ChiakiControllerButton bit layout
	uint8_t l2;
	uint8_t r2;
	int16_t left_x;
	int16_t left_y;
	int16_t right_x;
	int16_t right_y;
	uint8_t touch_active[2];
	uint16_t touch_x[2]; // 0..1919
	uint16_t touch_y[2]; // 0..1079
	int has_motion;
	float gyro_x, gyro_y, gyro_z;    // rad/s
	float accel_x, accel_y, accel_z; // g
	uint32_t timestamp_us;
} PslcControllerInput;

// Sends controller input to the console (only when it changed). Thread-safe.
PSLC_API void pslc_session_set_controller(PslcSession *session, const PslcControllerInput *input);
PSLC_API void pslc_session_set_login_pin(PslcSession *session, const char *pin);
// Stops the session, joins all threads and frees everything. Callbacks are not invoked after it returns.
PSLC_API void pslc_session_free(PslcSession *session);

#define PSLC_FORMAT_NONE 0
#define PSLC_FORMAT_NV12 1
#define PSLC_FORMAT_P010 2

typedef struct PslcFrameInfo
{
	uint32_t width;
	uint32_t height;
	int format;
	int hw;
	uint64_t sequence;
} PslcFrameInfo;

PSLC_API int pslc_session_frame_info(PslcSession *session, PslcFrameInfo *out);
// Copies the newest decoded frame into dst (ID3D11Texture2D* of matching size and format,
// D3D11_USAGE_DEFAULT). Returns 1 when a frame newer than *sequence was copied and updates *sequence.
// Must be called from the thread that owns rendering on the D3D11 device.
PSLC_API int pslc_session_copy_frame(PslcSession *session, void *dst_texture, uint64_t *sequence);

typedef struct PslcStats
{
	uint64_t frames_received;
	uint64_t frames_decoded;
	uint64_t frames_lost;
	uint64_t frames_dropped;
	uint64_t decode_errors;
	uint64_t video_bytes;
	uint64_t audio_frames;
	uint64_t rtt_us;
	double packet_loss;
} PslcStats;

PSLC_API void pslc_session_stats(PslcSession *session, PslcStats *out);

#ifdef __cplusplus
}
#endif

#endif
