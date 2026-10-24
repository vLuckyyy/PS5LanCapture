// SPDX-License-Identifier: AGPL-3.0-only
#define PSLC_BUILD
#define COBJMACROS
#define WIN32_LEAN_AND_MEAN

#include "pslc.h"

#include <winsock2.h>
#include <ws2tcpip.h>
#include <iphlpapi.h>
#include <windows.h>
#include <d3d11.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdarg.h>

#include <chiaki/common.h>
#include <chiaki/log.h>
#include <chiaki/session.h>
#include <chiaki/regist.h>
#include <chiaki/discovery.h>
#include <chiaki/discoveryservice.h>
#include <chiaki/opusdecoder.h>
#include <chiaki/controller.h>
#include <chiaki/orientation.h>
#include <chiaki/time.h>

#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d11va.h>
#include <libavutil/imgutils.h>

#define PSLC_MAX_HOSTS 16

static PslcLogCb g_log_cb = NULL;
static void *g_log_user = NULL;
static ChiakiLog g_log;
static int g_initialized = 0;

static int pslc_level_from_chiaki(ChiakiLogLevel level)
{
	switch(level)
	{
		case CHIAKI_LOG_ERROR: return PSLC_LOG_ERROR;
		case CHIAKI_LOG_WARNING: return PSLC_LOG_WARNING;
		case CHIAKI_LOG_INFO: return PSLC_LOG_INFO;
		default: return PSLC_LOG_DEBUG;
	}
}

static void pslc_forward_log(int level, const char *msg)
{
	if(g_log_cb)
		g_log_cb(level, msg, g_log_user);
}

static void pslc_logf(int level, const char *fmt, ...)
{
	char buf[1024];
	va_list args;
	va_start(args, fmt);
	vsnprintf(buf, sizeof(buf), fmt, args);
	va_end(args);
	pslc_forward_log(level, buf);
}

static void pslc_chiaki_log_cb(ChiakiLogLevel level, const char *msg, void *user)
{
	(void)user;
	pslc_forward_log(pslc_level_from_chiaki(level), msg);
}

static void pslc_av_log_cb(void *avcl, int level, const char *fmt, va_list vl)
{
	(void)avcl;
	if(level > AV_LOG_WARNING)
		return;
	char buf[1024];
	vsnprintf(buf, sizeof(buf), fmt, vl);
	size_t len = strlen(buf);
	while(len > 0 && (buf[len - 1] == '\n' || buf[len - 1] == '\r'))
		buf[--len] = '\0';
	if(len > 0)
		pslc_forward_log(level <= AV_LOG_ERROR ? PSLC_LOG_ERROR : PSLC_LOG_WARNING, buf);
}

PSLC_API int pslc_init(PslcLogCb log_cb, void *user)
{
	g_log_cb = log_cb;
	g_log_user = user;
	if(g_initialized)
		return 0;
	chiaki_log_init(&g_log, CHIAKI_LOG_ALL & ~(CHIAKI_LOG_VERBOSE | CHIAKI_LOG_DEBUG), pslc_chiaki_log_cb, NULL);
	ChiakiErrorCode err = chiaki_lib_init();
	if(err != CHIAKI_ERR_SUCCESS)
	{
		pslc_logf(PSLC_LOG_ERROR, "chiaki_lib_init failed: %s", chiaki_error_string(err));
		return -1;
	}
	av_log_set_callback(pslc_av_log_cb);
	g_initialized = 1;
	return 0;
}

PSLC_API const char *pslc_version(void)
{
	return "chiaki-ng " PSLC_CHIAKI_VERSION " / libavcodec " AV_STRINGIFY(LIBAVCODEC_VERSION);
}

static void pslc_copy_str(char *dst, size_t dst_size, const char *src)
{
	if(!dst_size)
		return;
	if(!src)
	{
		dst[0] = '\0';
		return;
	}
	strncpy(dst, src, dst_size - 1);
	dst[dst_size - 1] = '\0';
}

struct PslcDiscovery
{
	ChiakiDiscoveryService service;
	CRITICAL_SECTION lock;
	PslcHost hosts[PSLC_MAX_HOSTS];
	int hosts_count;
};

static void pslc_discovery_cb(ChiakiDiscoveryHost *hosts, size_t hosts_count, void *user)
{
	PslcDiscovery *discovery = user;
	EnterCriticalSection(&discovery->lock);
	int count = 0;
	for(size_t i = 0; i < hosts_count && count < PSLC_MAX_HOSTS; i++)
	{
		ChiakiDiscoveryHost *src = &hosts[i];
		PslcHost *dst = &discovery->hosts[count++];
		memset(dst, 0, sizeof(*dst));
		pslc_copy_str(dst->addr, sizeof(dst->addr), src->host_addr);
		pslc_copy_str(dst->name, sizeof(dst->name), src->host_name);
		pslc_copy_str(dst->host_id, sizeof(dst->host_id), src->host_id);
		pslc_copy_str(dst->host_type, sizeof(dst->host_type), src->host_type);
		pslc_copy_str(dst->system_version, sizeof(dst->system_version), src->system_version);
		pslc_copy_str(dst->running_app, sizeof(dst->running_app), src->running_app_name);
		dst->state = (int)src->state;
		dst->is_ps5 = chiaki_discovery_host_is_ps5(src) ? 1 : 0;
		dst->target = (int)chiaki_discovery_host_system_version_target(src);
	}
	discovery->hosts_count = count;
	LeaveCriticalSection(&discovery->lock);
}

static size_t pslc_collect_broadcast_addrs(struct sockaddr_storage *out, size_t max)
{
	ULONG size = 16 * 1024;
	IP_ADAPTER_INFO *info = NULL;
	DWORD ret = ERROR_BUFFER_OVERFLOW;
	for(int attempt = 0; attempt < 3 && ret == ERROR_BUFFER_OVERFLOW; attempt++)
	{
		free(info);
		info = malloc(size);
		if(!info)
			return 0;
		ret = GetAdaptersInfo(info, &size);
	}
	size_t count = 0;
	if(ret == NO_ERROR)
	{
		for(IP_ADAPTER_INFO *adapter = info; adapter && count < max; adapter = adapter->Next)
		{
			for(IP_ADDR_STRING *ip = &adapter->IpAddressList; ip && count < max; ip = ip->Next)
			{
				struct in_addr addr, mask;
				if(strcmp(ip->IpAddress.String, "0.0.0.0") == 0)
					continue;
				if(inet_pton(AF_INET, ip->IpAddress.String, &addr) != 1 || inet_pton(AF_INET, ip->IpMask.String, &mask) != 1)
					continue;
				uint32_t broadcast = addr.s_addr | ~mask.s_addr;
				int duplicate = 0;
				for(size_t i = 0; i < count; i++)
					if(((struct sockaddr_in *)&out[i])->sin_addr.s_addr == broadcast)
						duplicate = 1;
				if(duplicate)
					continue;
				struct sockaddr_in in = { 0 };
				in.sin_family = AF_INET;
				in.sin_addr.s_addr = broadcast;
				memset(&out[count], 0, sizeof(out[count]));
				memcpy(&out[count], &in, sizeof(in));
				count++;
			}
		}
	}
	free(info);
	return count;
}

PSLC_API PslcDiscovery *pslc_discovery_start(const char *unicast_host)
{
	PslcDiscovery *discovery = calloc(1, sizeof(PslcDiscovery));
	if(!discovery)
		return NULL;
	InitializeCriticalSection(&discovery->lock);

	ChiakiDiscoveryServiceOptions options = { 0 };
	options.ping_ms = 500;
	options.hosts_max = PSLC_MAX_HOSTS;
	options.host_drop_pings = 6;
	options.cb = pslc_discovery_cb;
	options.cb_user = discovery;

	struct sockaddr_in in = { 0 };
	in.sin_family = AF_INET;
	struct sockaddr_storage send_addr = { 0 };
	struct sockaddr_storage broadcast_addrs[16];

	if(unicast_host && unicast_host[0])
	{
		struct addrinfo hints = { 0 }, *res = NULL;
		hints.ai_family = AF_INET;
		hints.ai_socktype = SOCK_DGRAM;
		if(getaddrinfo(unicast_host, NULL, &hints, &res) != 0 || !res)
		{
			pslc_logf(PSLC_LOG_ERROR, "Could not resolve host %s", unicast_host);
			DeleteCriticalSection(&discovery->lock);
			free(discovery);
			return NULL;
		}
		memcpy(&in, res->ai_addr, sizeof(in));
		freeaddrinfo(res);
		options.send_host = (char *)unicast_host;
	}
	else
	{
		in.sin_addr.s_addr = 0xffffffff;
		options.broadcast_num = pslc_collect_broadcast_addrs(broadcast_addrs, 16);
		options.broadcast_addrs = options.broadcast_num ? broadcast_addrs : NULL;
	}
	memcpy(&send_addr, &in, sizeof(in));
	options.send_addr = &send_addr;
	options.send_addr_size = sizeof(in);

	ChiakiErrorCode err = chiaki_discovery_service_init(&discovery->service, &options, &g_log);
	if(err != CHIAKI_ERR_SUCCESS)
	{
		pslc_logf(PSLC_LOG_ERROR, "Discovery init failed: %s", chiaki_error_string(err));
		DeleteCriticalSection(&discovery->lock);
		free(discovery);
		return NULL;
	}
	return discovery;
}

PSLC_API int pslc_discovery_hosts(PslcDiscovery *discovery, PslcHost *out, int max)
{
	if(!discovery || !out || max <= 0)
		return 0;
	EnterCriticalSection(&discovery->lock);
	int count = discovery->hosts_count < max ? discovery->hosts_count : max;
	memcpy(out, discovery->hosts, (size_t)count * sizeof(PslcHost));
	LeaveCriticalSection(&discovery->lock);
	return count;
}

PSLC_API void pslc_discovery_stop(PslcDiscovery *discovery)
{
	if(!discovery)
		return;
	chiaki_discovery_service_fini(&discovery->service);
	DeleteCriticalSection(&discovery->lock);
	free(discovery);
}

PSLC_API int pslc_wakeup(const char *host, const char *regist_key, int ps5)
{
	char key[17] = { 0 };
	memcpy(key, regist_key, 16);
	for(int i = 0; i < 16; i++)
	{
		if(key[i] == '\0')
			break;
		if(i >= 8)
		{
			key[8] = '\0';
			break;
		}
	}
	char *end = NULL;
	uint64_t credential = _strtoui64(key, &end, 16);
	if(!end || end == key)
	{
		pslc_logf(PSLC_LOG_ERROR, "Invalid regist key for wakeup");
		return -1;
	}
	ChiakiErrorCode err = chiaki_discovery_wakeup(&g_log, NULL, host, credential, ps5 != 0);
	if(err != CHIAKI_ERR_SUCCESS)
	{
		pslc_logf(PSLC_LOG_ERROR, "Wakeup failed: %s", chiaki_error_string(err));
		return -1;
	}
	return 0;
}

struct PslcRegist
{
	ChiakiRegist regist;
	ChiakiLog log;
	CRITICAL_SECTION lock;
	PslcRegistResult result;
	char host[256];
	int started;
};

static void pslc_regist_log_cb(ChiakiLogLevel level, const char *msg, void *user)
{
	PslcRegist *regist = user;
	if(level == CHIAKI_LOG_ERROR)
	{
		EnterCriticalSection(&regist->lock);
		pslc_copy_str(regist->result.error, sizeof(regist->result.error), msg);
		LeaveCriticalSection(&regist->lock);
	}
	pslc_forward_log(pslc_level_from_chiaki(level), msg);
}

static void pslc_regist_cb(ChiakiRegistEvent *event, void *user)
{
	PslcRegist *regist = user;
	EnterCriticalSection(&regist->lock);
	switch(event->type)
	{
		case CHIAKI_REGIST_EVENT_TYPE_FINISHED_SUCCESS:
		{
			ChiakiRegisteredHost *host = event->registered_host;
			regist->result.status = 1;
			regist->result.target = (int)host->target;
			pslc_copy_str(regist->result.nickname, sizeof(regist->result.nickname), host->server_nickname);
			memcpy(regist->result.regist_key, host->rp_regist_key, sizeof(regist->result.regist_key));
			memcpy(regist->result.rp_key, host->rp_key, sizeof(regist->result.rp_key));
			regist->result.rp_key_type = host->rp_key_type;
			memcpy(regist->result.server_mac, host->server_mac, sizeof(regist->result.server_mac));
			break;
		}
		case CHIAKI_REGIST_EVENT_TYPE_FINISHED_CANCELED:
			regist->result.status = 3;
			break;
		default:
			regist->result.status = 2;
			if(!regist->result.error[0])
				pslc_copy_str(regist->result.error, sizeof(regist->result.error), "Registration failed");
			break;
	}
	LeaveCriticalSection(&regist->lock);
}

PSLC_API PslcRegist *pslc_regist_start(const char *host, int broadcast, int target, const uint8_t account_id[8], uint32_t pin, uint32_t console_pin)
{
	PslcRegist *regist = calloc(1, sizeof(PslcRegist));
	if(!regist)
		return NULL;
	InitializeCriticalSection(&regist->lock);
	chiaki_log_init(&regist->log, CHIAKI_LOG_ALL & ~(CHIAKI_LOG_VERBOSE | CHIAKI_LOG_DEBUG), pslc_regist_log_cb, regist);
	pslc_copy_str(regist->host, sizeof(regist->host), host);

	ChiakiRegistInfo info = { 0 };
	info.target = (ChiakiTarget)target;
	info.host = regist->host;
	info.broadcast = broadcast != 0;
	info.psn_online_id = NULL;
	memcpy(info.psn_account_id, account_id, CHIAKI_PSN_ACCOUNT_ID_SIZE);
	info.pin = pin;
	info.console_pin = console_pin;
	info.holepunch_info = NULL;
	info.rudp = NULL;

	ChiakiErrorCode err = chiaki_regist_start(&regist->regist, &regist->log, &info, pslc_regist_cb, regist);
	if(err != CHIAKI_ERR_SUCCESS)
	{
		regist->result.status = 2;
		snprintf(regist->result.error, sizeof(regist->result.error), "Registration start failed: %s", chiaki_error_string(err));
		return regist;
	}
	regist->started = 1;
	return regist;
}

PSLC_API int pslc_regist_poll(PslcRegist *regist, PslcRegistResult *out)
{
	if(!regist)
		return -1;
	EnterCriticalSection(&regist->lock);
	*out = regist->result;
	LeaveCriticalSection(&regist->lock);
	return out->status != 0;
}

PSLC_API void pslc_regist_free(PslcRegist *regist)
{
	if(!regist)
		return;
	if(regist->started)
	{
		chiaki_regist_stop(&regist->regist);
		chiaki_regist_fini(&regist->regist);
	}
	DeleteCriticalSection(&regist->lock);
	free(regist);
}

typedef struct PslcDecoder
{
	AVCodecContext *ctx;
	AVBufferRef *hw_device;
	AVFrame *latest;
	AVFrame *scratch;
	AVPacket *packet;
	CRITICAL_SECTION lock;
	uint64_t sequence;
	uint64_t copied_sequence;
	int hw;
	int failed;
	ID3D11Device *device;
	ID3D11DeviceContext *device_context;
	ID3D11Texture2D *staging;
	uint32_t staging_width;
	uint32_t staging_height;
} PslcDecoder;

static enum AVPixelFormat pslc_get_format(AVCodecContext *ctx, const enum AVPixelFormat *formats)
{
	for(const enum AVPixelFormat *p = formats; *p != AV_PIX_FMT_NONE; p++)
		if(*p == AV_PIX_FMT_D3D11)
			return *p;
	pslc_logf(PSLC_LOG_WARNING, "D3D11 hardware surface format not offered, falling back to software decoding");
	PslcDecoder *decoder = ctx->opaque;
	if(decoder)
		decoder->hw = 0;
	return avcodec_default_get_format(ctx, formats);
}

static int pslc_decoder_open(PslcDecoder *decoder, int codec, int prefer_hw, ID3D11Device *device)
{
	InitializeCriticalSection(&decoder->lock);
	decoder->device = device;
	if(device)
	{
		ID3D11Device_AddRef(device);
		ID3D11Device_GetImmediateContext(device, &decoder->device_context);
	}

	const AVCodec *av_codec = avcodec_find_decoder(codec == PSLC_CODEC_H265 ? AV_CODEC_ID_HEVC : AV_CODEC_ID_H264);
	if(!av_codec)
	{
		pslc_logf(PSLC_LOG_ERROR, "FFmpeg decoder for %s not available", codec == PSLC_CODEC_H265 ? "HEVC" : "H.264");
		return -1;
	}
	decoder->ctx = avcodec_alloc_context3(av_codec);
	decoder->latest = av_frame_alloc();
	decoder->scratch = av_frame_alloc();
	decoder->packet = av_packet_alloc();
	if(!decoder->ctx || !decoder->latest || !decoder->scratch || !decoder->packet)
		return -1;

	decoder->ctx->opaque = decoder;
	decoder->ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;
	decoder->ctx->flags2 |= AV_CODEC_FLAG2_FAST;

	if(prefer_hw && device)
	{
		decoder->hw_device = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);
		if(decoder->hw_device)
		{
			AVHWDeviceContext *hw_ctx = (AVHWDeviceContext *)decoder->hw_device->data;
			AVD3D11VADeviceContext *d3d_ctx = hw_ctx->hwctx;
			ID3D11Device_AddRef(device);
			d3d_ctx->device = device;
			if(av_hwdevice_ctx_init(decoder->hw_device) < 0)
			{
				pslc_logf(PSLC_LOG_WARNING, "D3D11VA device init failed, using software decoding");
				av_buffer_unref(&decoder->hw_device);
			}
		}
		if(decoder->hw_device)
		{
			decoder->ctx->hw_device_ctx = av_buffer_ref(decoder->hw_device);
			decoder->ctx->get_format = pslc_get_format;
			decoder->ctx->extra_hw_frames = 3;
			decoder->hw = 1;
		}
	}
	if(!decoder->hw)
	{
		decoder->ctx->thread_type = FF_THREAD_SLICE;
		decoder->ctx->thread_count = 0;
	}

	if(avcodec_open2(decoder->ctx, av_codec, NULL) < 0)
	{
		pslc_logf(PSLC_LOG_ERROR, "avcodec_open2 failed");
		return -1;
	}
	pslc_logf(PSLC_LOG_INFO, "Video decoder: %s (%s)", av_codec->name, decoder->hw ? "D3D11VA" : "software");
	return 0;
}

static void pslc_decoder_close(PslcDecoder *decoder)
{
	avcodec_free_context(&decoder->ctx);
	av_frame_free(&decoder->latest);
	av_frame_free(&decoder->scratch);
	av_packet_free(&decoder->packet);
	av_buffer_unref(&decoder->hw_device);
	if(decoder->staging)
		ID3D11Texture2D_Release(decoder->staging);
	if(decoder->device_context)
		ID3D11DeviceContext_Release(decoder->device_context);
	if(decoder->device)
		ID3D11Device_Release(decoder->device);
	DeleteCriticalSection(&decoder->lock);
}

// Returns number of frames produced, or -1 on error.
static int pslc_decoder_push(PslcDecoder *decoder, uint8_t *buf, size_t size, uint64_t *dropped)
{
	decoder->packet->data = buf;
	decoder->packet->size = (int)size;
	int r = avcodec_send_packet(decoder->ctx, decoder->packet);
	if(r == AVERROR(EAGAIN))
	{
		while(avcodec_receive_frame(decoder->ctx, decoder->scratch) == 0)
		{
			av_frame_unref(decoder->scratch);
			(*dropped)++;
		}
		r = avcodec_send_packet(decoder->ctx, decoder->packet);
	}
	decoder->packet->data = NULL;
	decoder->packet->size = 0;
	if(r < 0)
	{
		char errbuf[128];
		av_make_error_string(errbuf, sizeof(errbuf), r);
		pslc_logf(PSLC_LOG_WARNING, "Decoder rejected packet: %s", errbuf);
		return -1;
	}
	int produced = 0;
	while((r = avcodec_receive_frame(decoder->ctx, decoder->scratch)) == 0)
	{
		EnterCriticalSection(&decoder->lock);
		if(decoder->latest->buf[0] && decoder->sequence != decoder->copied_sequence)
			(*dropped)++;
		av_frame_unref(decoder->latest);
		av_frame_move_ref(decoder->latest, decoder->scratch);
		decoder->sequence++;
		LeaveCriticalSection(&decoder->lock);
		produced++;
	}
	if(r != AVERROR(EAGAIN) && r != AVERROR_EOF)
		return -1;
	return produced;
}

static int pslc_format_of(const AVFrame *frame)
{
	if(frame->format == AV_PIX_FMT_D3D11)
	{
		if(!frame->hw_frames_ctx)
			return PSLC_FORMAT_NONE;
		AVHWFramesContext *frames = (AVHWFramesContext *)frame->hw_frames_ctx->data;
		return frames->sw_format == AV_PIX_FMT_P010 ? PSLC_FORMAT_P010 : PSLC_FORMAT_NV12;
	}
	switch(frame->format)
	{
		case AV_PIX_FMT_YUV420P:
		case AV_PIX_FMT_YUVJ420P:
		case AV_PIX_FMT_NV12:
			return PSLC_FORMAT_NV12;
		default:
			return PSLC_FORMAT_NONE;
	}
}

static int pslc_upload_software(PslcDecoder *decoder, const AVFrame *frame, ID3D11Texture2D *dst)
{
	uint32_t width = (uint32_t)frame->width & ~1u;
	uint32_t height = (uint32_t)frame->height & ~1u;
	if(!decoder->device || !decoder->device_context)
		return 0;
	if(!decoder->staging || decoder->staging_width != width || decoder->staging_height != height)
	{
		if(decoder->staging)
		{
			ID3D11Texture2D_Release(decoder->staging);
			decoder->staging = NULL;
		}
		D3D11_TEXTURE2D_DESC desc = { 0 };
		desc.Width = width;
		desc.Height = height;
		desc.MipLevels = 1;
		desc.ArraySize = 1;
		desc.Format = DXGI_FORMAT_NV12;
		desc.SampleDesc.Count = 1;
		desc.Usage = D3D11_USAGE_STAGING;
		desc.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
		if(FAILED(ID3D11Device_CreateTexture2D(decoder->device, &desc, NULL, &decoder->staging)))
			return 0;
		decoder->staging_width = width;
		decoder->staging_height = height;
	}
	D3D11_MAPPED_SUBRESOURCE mapped;
	if(FAILED(ID3D11DeviceContext_Map(decoder->device_context, (ID3D11Resource *)decoder->staging, 0, D3D11_MAP_WRITE, 0, &mapped)))
		return 0;
	uint8_t *y_dst = mapped.pData;
	uint8_t *uv_dst = y_dst + (size_t)mapped.RowPitch * height;
	for(uint32_t row = 0; row < height; row++)
		memcpy(y_dst + (size_t)row * mapped.RowPitch, frame->data[0] + (size_t)row * frame->linesize[0], width);
	if(frame->format == AV_PIX_FMT_NV12)
	{
		for(uint32_t row = 0; row < height / 2; row++)
			memcpy(uv_dst + (size_t)row * mapped.RowPitch, frame->data[1] + (size_t)row * frame->linesize[1], width);
	}
	else
	{
		for(uint32_t row = 0; row < height / 2; row++)
		{
			const uint8_t *u = frame->data[1] + (size_t)row * frame->linesize[1];
			const uint8_t *v = frame->data[2] + (size_t)row * frame->linesize[2];
			uint8_t *out = uv_dst + (size_t)row * mapped.RowPitch;
			for(uint32_t col = 0; col < width / 2; col++)
			{
				out[col * 2] = u[col];
				out[col * 2 + 1] = v[col];
			}
		}
	}
	ID3D11DeviceContext_Unmap(decoder->device_context, (ID3D11Resource *)decoder->staging, 0);
	ID3D11DeviceContext_CopyResource(decoder->device_context, (ID3D11Resource *)dst, (ID3D11Resource *)decoder->staging);
	return 1;
}

struct PslcSession
{
	ChiakiSession session;
	ChiakiOpusDecoder opus;
	PslcDecoder decoder;
	PslcEventCb event_cb;
	PslcAudioCb audio_cb;
	PslcFeedbackCb feedback_cb;
	void *user;
	char host[256];
	CRITICAL_SECTION input_lock;
	int input_lock_inited;
	ChiakiControllerState controller_state;
	ChiakiOrientationTracker orient_tracker;
	ChiakiAccelNewZero accel_zero;
	int motion_started;
	int8_t touch_ids[2];
	uint32_t audio_channels;
	uint32_t audio_rate;
	volatile LONG64 frames_received;
	volatile LONG64 frames_decoded;
	volatile LONG64 frames_lost;
	volatile LONG64 frames_dropped;
	volatile LONG64 decode_errors;
	volatile LONG64 video_bytes;
	volatile LONG64 audio_frames;
	int session_started;
	int session_inited;
	int opus_inited;
	int decoder_inited;
};

static void pslc_emit(PslcSession *s, int type, int code, int is_error, const char *msg)
{
	if(s->event_cb)
		s->event_cb(type, code, is_error, msg ? msg : "", s->user);
}

static void pslc_feedback(PslcSession *s, int kind, const uint8_t *data, size_t size)
{
	if(s->feedback_cb)
		s->feedback_cb(kind, data, size, s->user);
}

// DualSense haptics arrive as a 3 kHz stereo int16 stream; like chiaki-ng we reduce each packet to
// a rumble strength per side so haptics-only games still vibrate a pad connected to the PC.
static void pslc_haptics_frame_cb(uint8_t *buf, size_t buf_size, void *user)
{
	PslcSession *s = user;
	size_t frames = buf_size / 4;
	if(!frames)
		return;
	uint32_t sum_left = 0, sum_right = 0;
	for(size_t i = 0; i < frames; i++)
	{
		int16_t left, right;
		memcpy(&left, buf + i * 4, 2);
		memcpy(&right, buf + i * 4 + 2, 2);
		sum_left += (uint32_t)abs(left) * 2;
		sum_right += (uint32_t)abs(right) * 2;
	}
	uint32_t left = sum_left / (uint32_t)frames;
	uint32_t right = sum_right / (uint32_t)frames;
	left = left > 100 ? left : 0;
	right = right > 100 ? right : 0;
	uint8_t data[2] = { (uint8_t)(left > 65535 ? 255 : left >> 8), (uint8_t)(right > 65535 ? 255 : right >> 8) };
	pslc_feedback(s, PSLC_FEEDBACK_HAPTIC_RUMBLE, data, sizeof(data));
}

static void pslc_session_event_cb(ChiakiEvent *event, void *user)
{
	PslcSession *s = user;
	switch(event->type)
	{
		case CHIAKI_EVENT_CONNECTED:
			pslc_emit(s, PSLC_EVENT_CONNECTED, 0, 0, NULL);
			break;
		case CHIAKI_EVENT_LOGIN_PIN_REQUEST:
			pslc_emit(s, PSLC_EVENT_LOGIN_PIN_REQUEST, event->login_pin_request.pin_incorrect ? 1 : 0, 0, NULL);
			break;
		case CHIAKI_EVENT_NICKNAME_RECEIVED:
			pslc_emit(s, PSLC_EVENT_NICKNAME, 0, 0, event->server_nickname);
			break;
		case CHIAKI_EVENT_RUMBLE:
		{
			uint8_t data[2] = { event->rumble.left, event->rumble.right };
			pslc_feedback(s, PSLC_FEEDBACK_RUMBLE, data, sizeof(data));
			break;
		}
		case CHIAKI_EVENT_TRIGGER_EFFECTS:
		{
			uint8_t data[22];
			data[0] = event->trigger_effects.type_left;
			memcpy(data + 1, event->trigger_effects.left, 10);
			data[11] = event->trigger_effects.type_right;
			memcpy(data + 12, event->trigger_effects.right, 10);
			pslc_feedback(s, PSLC_FEEDBACK_TRIGGER_EFFECTS, data, sizeof(data));
			break;
		}
		case CHIAKI_EVENT_LED_COLOR:
			pslc_feedback(s, PSLC_FEEDBACK_LED, event->led_state, 3);
			break;
		case CHIAKI_EVENT_PLAYER_INDEX:
			pslc_feedback(s, PSLC_FEEDBACK_PLAYER_INDEX, &event->player_index, 1);
			break;
		case CHIAKI_EVENT_MOTION_RESET:
			EnterCriticalSection(&s->input_lock);
			s->motion_started = 0;
			chiaki_orientation_tracker_init(&s->orient_tracker);
			LeaveCriticalSection(&s->input_lock);
			pslc_feedback(s, PSLC_FEEDBACK_MOTION_RESET, NULL, 0);
			break;
		case CHIAKI_EVENT_QUIT:
		{
			char msg[512];
			snprintf(msg, sizeof(msg), "%s%s%s", chiaki_quit_reason_string(event->quit.reason),
					event->quit.reason_str ? ": " : "", event->quit.reason_str ? event->quit.reason_str : "");
			pslc_emit(s, PSLC_EVENT_QUIT, (int)event->quit.reason, chiaki_quit_reason_is_error(event->quit.reason) ? 1 : 0, msg);
			break;
		}
		default:
			break;
	}
}

static bool pslc_video_cb(uint8_t *buf, size_t buf_size, int32_t frames_lost, bool frame_recovered, void *user)
{
	(void)frame_recovered;
	PslcSession *s = user;
	InterlockedIncrement64(&s->frames_received);
	InterlockedAdd64(&s->video_bytes, (LONG64)buf_size);
	if(frames_lost > 0)
		InterlockedAdd64(&s->frames_lost, frames_lost);
	uint64_t dropped = 0;
	int produced = pslc_decoder_push(&s->decoder, buf, buf_size, &dropped);
	if(dropped)
		InterlockedAdd64(&s->frames_dropped, (LONG64)dropped);
	if(produced < 0)
	{
		InterlockedIncrement64(&s->decode_errors);
		return false;
	}
	InterlockedAdd64(&s->frames_decoded, produced);
	return true;
}

static void pslc_audio_settings_cb(uint32_t channels, uint32_t rate, void *user)
{
	PslcSession *s = user;
	s->audio_channels = channels;
	s->audio_rate = rate;
	pslc_logf(PSLC_LOG_INFO, "Audio stream: %u channels, %u Hz", channels, rate);
}

static void pslc_audio_frame_cb(int16_t *buf, size_t samples_count, void *user)
{
	PslcSession *s = user;
	InterlockedIncrement64(&s->audio_frames);
	if(s->audio_cb && s->audio_channels)
		s->audio_cb(buf, samples_count, s->audio_channels, s->audio_rate, s->user);
}

static void pslc_session_cleanup(PslcSession *s)
{
	if(s->session_started)
	{
		chiaki_session_stop(&s->session);
		chiaki_session_join(&s->session);
	}
	if(s->session_inited)
		chiaki_session_fini(&s->session);
	if(s->opus_inited)
		chiaki_opus_decoder_fini(&s->opus);
	if(s->decoder_inited)
		pslc_decoder_close(&s->decoder);
	if(s->input_lock_inited)
		DeleteCriticalSection(&s->input_lock);
	free(s);
}

PSLC_API PslcSession *pslc_session_start(const PslcSessionConfig *config, PslcEventCb event_cb, PslcAudioCb audio_cb, PslcFeedbackCb feedback_cb, void *user, char *err_out, size_t err_size)
{
	PslcSession *s = calloc(1, sizeof(PslcSession));
	if(!s)
	{
		snprintf(err_out, err_size, "Out of memory");
		return NULL;
	}
	s->event_cb = event_cb;
	s->audio_cb = audio_cb;
	s->feedback_cb = feedback_cb;
	s->user = user;
	pslc_copy_str(s->host, sizeof(s->host), config->host);
	InitializeCriticalSection(&s->input_lock);
	s->input_lock_inited = 1;
	chiaki_controller_state_set_idle(&s->controller_state);
	chiaki_orientation_tracker_init(&s->orient_tracker);
	chiaki_accel_new_zero_set_inactive(&s->accel_zero, false);
	s->touch_ids[0] = s->touch_ids[1] = -1;

	int codec = config->ps5 ? config->codec : PSLC_CODEC_H264;
	s->decoder_inited = 1;
	if(pslc_decoder_open(&s->decoder, codec, config->prefer_hw_decode, (ID3D11Device *)config->d3d11_device) != 0)
	{
		snprintf(err_out, err_size, "Could not initialize the video decoder");
		pslc_session_cleanup(s);
		return NULL;
	}
	pslc_emit(s, PSLC_EVENT_DECODER, s->decoder.hw, 0, s->decoder.hw ? "D3D11VA" : "software");

	chiaki_opus_decoder_init(&s->opus, &g_log);
	s->opus_inited = 1;

	ChiakiConnectInfo info;
	memset(&info, 0, sizeof(info));
	info.ps5 = config->ps5 != 0;
	info.host = s->host;
	memcpy(info.regist_key, config->regist_key, sizeof(info.regist_key));
	memcpy(info.morning, config->morning, sizeof(info.morning));
	info.video_profile.width = config->width;
	info.video_profile.height = config->height;
	info.video_profile.max_fps = config->fps;
	info.video_profile.bitrate = config->bitrate;
	info.video_profile.codec = codec == PSLC_CODEC_H265 ? CHIAKI_CODEC_H265 : CHIAKI_CODEC_H264;
	info.video_profile_auto_downgrade = true;
	info.enable_keyboard = false;
	info.enable_dualsense = true;
	info.audio_video_disabled = 0;
	info.auto_regist = false;
	info.holepunch_session = NULL;
	info.rudp_sock = NULL;
	info.packet_loss_max = config->packet_loss_max > 0 ? config->packet_loss_max : 0.05;
	info.enable_idr_on_fec_failure = true;

	ChiakiErrorCode err = chiaki_session_init(&s->session, &info, &g_log);
	if(err != CHIAKI_ERR_SUCCESS)
	{
		snprintf(err_out, err_size, "Session init failed: %s", chiaki_error_string(err));
		pslc_session_cleanup(s);
		return NULL;
	}
	s->session_inited = 1;

	chiaki_opus_decoder_set_cb(&s->opus, pslc_audio_settings_cb, pslc_audio_frame_cb, s);
	ChiakiAudioSink sink;
	chiaki_opus_decoder_get_sink(&s->opus, &sink);
	chiaki_session_set_audio_sink(&s->session, &sink);
	ChiakiAudioSink haptics_sink = { 0 };
	haptics_sink.user = s;
	haptics_sink.frame_cb = pslc_haptics_frame_cb;
	chiaki_session_set_haptics_sink(&s->session, &haptics_sink);
	chiaki_session_set_video_sample_cb(&s->session, pslc_video_cb, s);
	chiaki_session_set_event_cb(&s->session, pslc_session_event_cb, s);

	err = chiaki_session_start(&s->session);
	if(err != CHIAKI_ERR_SUCCESS)
	{
		snprintf(err_out, err_size, "Session start failed: %s", chiaki_error_string(err));
		pslc_session_cleanup(s);
		return NULL;
	}
	s->session_started = 1;
	return s;
}

PSLC_API void pslc_session_set_controller(PslcSession *s, const PslcControllerInput *in)
{
	if(!s || !in || !s->session_started || !s->input_lock_inited)
		return;
	EnterCriticalSection(&s->input_lock);
	ChiakiControllerState next = s->controller_state;
	next.buttons = in->buttons & 0xffff;
	next.l2_state = in->l2;
	next.r2_state = in->r2;
	next.left_x = in->left_x;
	next.left_y = in->left_y;
	next.right_x = in->right_x;
	next.right_y = in->right_y;
	for(int i = 0; i < 2; i++)
	{
		if(in->touch_active[i])
		{
			if(s->touch_ids[i] < 0)
				s->touch_ids[i] = chiaki_controller_state_start_touch(&next, in->touch_x[i], in->touch_y[i]);
			else
				chiaki_controller_state_set_touch_pos(&next, (uint8_t)s->touch_ids[i], in->touch_x[i], in->touch_y[i]);
		}
		else if(s->touch_ids[i] >= 0)
		{
			chiaki_controller_state_stop_touch(&next, (uint8_t)s->touch_ids[i]);
			s->touch_ids[i] = -1;
		}
	}
	if(in->has_motion)
	{
		if(!s->motion_started)
		{
			chiaki_accel_new_zero_set_active(&s->accel_zero, in->accel_x, in->accel_y, in->accel_z, false);
			chiaki_orientation_tracker_init(&s->orient_tracker);
			s->motion_started = 1;
		}
		chiaki_orientation_tracker_update(&s->orient_tracker, in->gyro_x, in->gyro_y, in->gyro_z,
				in->accel_x, in->accel_y, in->accel_z, &s->accel_zero, false, in->timestamp_us);
		chiaki_orientation_tracker_apply_to_controller_state(&s->orient_tracker, &next);
	}
	int changed = !chiaki_controller_state_equals(&next, &s->controller_state);
	s->controller_state = next;
	if(changed)
		chiaki_session_set_controller_state(&s->session, &s->controller_state);
	LeaveCriticalSection(&s->input_lock);
}

PSLC_API void pslc_session_set_login_pin(PslcSession *s, const char *pin)
{
	if(!s || !pin)
		return;
	chiaki_session_set_login_pin(&s->session, (const uint8_t *)pin, strlen(pin));
}

PSLC_API void pslc_session_free(PslcSession *s)
{
	if(!s)
		return;
	pslc_session_cleanup(s);
}

PSLC_API int pslc_session_frame_info(PslcSession *s, PslcFrameInfo *out)
{
	memset(out, 0, sizeof(*out));
	if(!s)
		return 0;
	EnterCriticalSection(&s->decoder.lock);
	if(s->decoder.latest->buf[0])
	{
		out->width = (uint32_t)s->decoder.latest->width & ~1u;
		out->height = (uint32_t)s->decoder.latest->height & ~1u;
		out->format = pslc_format_of(s->decoder.latest);
		out->hw = s->decoder.latest->format == AV_PIX_FMT_D3D11;
		out->sequence = s->decoder.sequence;
	}
	LeaveCriticalSection(&s->decoder.lock);
	return out->format != PSLC_FORMAT_NONE;
}

PSLC_API int pslc_session_copy_frame(PslcSession *s, void *dst_texture, uint64_t *sequence)
{
	if(!s || !dst_texture || !sequence)
		return 0;
	PslcDecoder *decoder = &s->decoder;
	AVFrame *frame = av_frame_alloc();
	if(!frame)
		return 0;
	uint64_t seq;
	EnterCriticalSection(&decoder->lock);
	seq = decoder->sequence;
	int have = decoder->latest->buf[0] && seq != *sequence;
	if(have && av_frame_ref(frame, decoder->latest) < 0)
		have = 0;
	LeaveCriticalSection(&decoder->lock);
	if(!have)
	{
		av_frame_free(&frame);
		return 0;
	}

	ID3D11Texture2D *dst = dst_texture;
	int copied = 0;
	if(frame->format == AV_PIX_FMT_D3D11)
	{
		ID3D11Texture2D *src = (ID3D11Texture2D *)frame->data[0];
		UINT index = (UINT)(intptr_t)frame->data[1];
		AVHWDeviceContext *hw_ctx = (AVHWDeviceContext *)decoder->hw_device->data;
		AVD3D11VADeviceContext *d3d_ctx = hw_ctx->hwctx;
		D3D11_BOX box = { 0, 0, 0, (UINT)frame->width & ~1u, (UINT)frame->height & ~1u, 1 };
		d3d_ctx->lock(d3d_ctx->lock_ctx);
		ID3D11DeviceContext_CopySubresourceRegion(decoder->device_context, (ID3D11Resource *)dst, 0, 0, 0, 0, (ID3D11Resource *)src, index, &box);
		d3d_ctx->unlock(d3d_ctx->lock_ctx);
		copied = 1;
	}
	else if(pslc_format_of(frame) == PSLC_FORMAT_NV12)
	{
		copied = pslc_upload_software(decoder, frame, dst);
	}
	av_frame_free(&frame);
	if(copied)
	{
		*sequence = seq;
		EnterCriticalSection(&decoder->lock);
		decoder->copied_sequence = seq;
		LeaveCriticalSection(&decoder->lock);
	}
	return copied;
}

PSLC_API void pslc_session_stats(PslcSession *s, PslcStats *out)
{
	memset(out, 0, sizeof(*out));
	if(!s)
		return;
	out->frames_received = (uint64_t)s->frames_received;
	out->frames_decoded = (uint64_t)s->frames_decoded;
	out->frames_lost = (uint64_t)s->frames_lost;
	out->frames_dropped = (uint64_t)s->frames_dropped;
	out->decode_errors = (uint64_t)s->decode_errors;
	out->video_bytes = (uint64_t)s->video_bytes;
	out->audio_frames = (uint64_t)s->audio_frames;
	out->rtt_us = s->session.rtt_us;
	uint64_t total = out->frames_received + out->frames_lost;
	out->packet_loss = total ? (double)out->frames_lost / (double)total : 0.0;
}
