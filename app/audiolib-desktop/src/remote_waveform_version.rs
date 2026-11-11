// app/audiolib-desktop/src/remote_waveform_version.rs
//! Strong original identities bind displayed Waveforms, audition and slice export.
use super::*;
use audiolib_remote::OriginalVersion;

impl Controller {
    /// Force disk-cache coverage and observe admission without exposing runtime state.
    #[cfg(test)]
    pub(in crate::controller) fn clear_remote_waveform_memory_for_test(&self) -> usize {
        let mut remote = self.remote.lock().unwrap();
        let count = remote.waveforms.len();
        remote.waveforms.clear();
        count
    }

    pub(in crate::controller) fn remote_original_version(
        &self,
        id: i64,
        cancel: &AtomicBool,
    ) -> Result<OriginalVersion, String> {
        let source = self.remote_drag_source()?;
        DownloadCache::new(source.cache_directory, DEFAULT_DOWNLOAD_CACHE_BYTES)
            .original_version(
                &source.state.library_id,
                id,
                &source.client.download_source(id),
                cancel,
            )
            .map_err(|error| error.localized(self.chinese.load(Ordering::Acquire)))
    }

    pub(in crate::controller) fn check_remote_original_version(
        &self,
        id: i64,
        expected: &OriginalVersion,
        cancel: &AtomicBool,
    ) -> Result<OriginalVersion, String> {
        let source = self.remote_drag_source()?;
        let current = DownloadCache::new(source.cache_directory, DEFAULT_DOWNLOAD_CACHE_BYTES)
            .original_version_expected(
                &source.state.library_id,
                id,
                &source.client.download_source(id),
                cancel,
                expected,
            )
            .map_err(|error| error.localized(self.chinese.load(Ordering::Acquire)))?;
        if current.etag != expected.etag || current.length != expected.length {
            return Err(self
                .localized(
                    "原素材已在波形绘制后改变，请重新载入音频",
                    "The original changed since this Waveform was rendered; reload the sound",
                )
                .into());
        }
        Ok(current)
    }

    pub(in crate::controller) fn remote_waveform_versioned_segment(
        &self,
        (id, rate): (i64, u32),
        (request, buckets): (WaveformRequest, u32),
        cancel: &AtomicBool,
        deferred_audio: &mut bool,
        expected: &OriginalVersion,
    ) -> Result<WaveformData, String> {
        let before = self.check_remote_original_version(id, expected, cancel)?;
        let source = self.remote_drag_source()?;
        let query = WaveformQuery {
            buckets: buckets.clamp(64, 8192),
            colors: request.include_colors,
            start_frame: Some(request.start_frame),
            end_frame: request.end_frame,
        };
        let key = format!(
            "source-v1-{}-{}-{}-{}-{}-{}",
            audiolib_remote::etag_token(&expected.etag),
            id,
            request.start_frame,
            request.end_frame.unwrap_or(u64::MAX),
            query.buckets,
            u8::from(query.colors)
        );
        // The exact ETag (not its filename hash) is part of every memory identity.
        let memory_key = format!(
            "{}/{}/{}/{}/{}",
            source.state.library_id,
            source.state.database_file,
            expected.etag,
            expected.length,
            key
        );
        let frames = request
            .end_frame
            .unwrap_or(0)
            .saturating_sub(request.start_frame);
        let cached = self
            .remote
            .lock()
            .unwrap()
            .waveforms
            .get(&memory_key)
            .cloned()
            .filter(|data| crate::progressive_waveform::validate_part(data, frames, rate).is_ok());
        if let Some(data) = cached {
            self.check_remote_original_version(id, expected, cancel)?;
            return Ok(data);
        }
        let library = RemoteCache::new(&self.data_root()).library_dir(&source.state.library_id);
        let directory = source
            .cache_directory
            .join("waveforms/server")
            .join(library.file_name().unwrap_or_default());
        let json_path = directory.join(format!("{key}.json"));
        let etag_path = directory.join(format!("{key}.etag"));
        let receipt_path = directory.join(format!("{key}.source"));
        let receipt = serde_json::to_vec(&(&expected.etag, expected.length))
            .map_err(|error| error.to_string())?;
        let receipt_matches = std::fs::metadata(&receipt_path)
            .is_ok_and(|metadata| metadata.len() <= 8192)
            && std::fs::read(&receipt_path).ok().as_deref() == Some(receipt.as_slice());
        let stored = receipt_matches
            .then(|| {
                std::fs::read(&json_path)
                    .ok()
                    .and_then(|json| decode_server_waveform(&json, &request, rate).ok())
            })
            .flatten();
        let known = stored
            .as_ref()
            .and_then(|_| std::fs::read_to_string(&etag_path).ok());
        if known.is_none() && !*deferred_audio && !before.offline {
            self.wait_for_audio_start(id, cancel);
            *deferred_audio = true;
        }
        let fetched = if before.offline {
            None
        } else {
            Some(source.client.waveform(id, &query, known.as_deref(), cancel))
        };
        let mut body = None;
        let data = match fetched {
            Some(Ok(WaveformReply::Data { json, etag })) => {
                let data = decode_server_waveform(&json, &request, rate)?;
                body = Some((json, etag));
                Some(data)
            }
            Some(Ok(WaveformReply::NotModified)) | None => stored,
            Some(Err(RemoteError::Cancelled)) => return Err("Cancelled".into()),
            Some(Err(_)) => stored,
        };
        let data = match data {
            Some(data) => data,
            None => {
                // A fallback must be the verified original, never an unrelated
                // or low-quality preview carrying an opaque representation ETag.
                let original =
                    self.remote_stream
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|playback| {
                            playback.file_id == id
                                && playback.stream.etag().as_deref() == Some(expected.etag.as_str())
                                && playback
                                    .stream
                                    .source()
                                    .url
                                    .split('?')
                                    .next()
                                    .is_some_and(|url| url.ends_with("/stream"))
                        });
                if !original {
                    return Err(self.localized("没有该已验证原素材版本的波形缓存，请连接服务器后重新载入", "No Waveform cache exists for this verified original version; reconnect and reload the sound").into());
                }
                self.stream_waveform(id, rate, request.clone(), cancel)?
            }
        };
        crate::progressive_waveform::validate_part(&data, frames, rate)?;
        // A same-size replacement during this segment must never be published,
        // cached under the previous source or mixed into progressive rendering.
        self.check_remote_original_version(id, expected, cancel)?;
        if let Some((json, etag)) = body
            && std::fs::create_dir_all(&directory).is_ok()
            && std::fs::write(&json_path, json).is_ok()
        {
            if let Some(etag) = etag {
                let _ = std::fs::write(&etag_path, etag);
            } else {
                let _ = std::fs::remove_file(&etag_path);
            }
            // Exact version receipt is published last, after both guards.
            let _ = std::fs::write(&receipt_path, receipt);
        }
        let mut remote = self.remote.lock().unwrap();
        if remote.waveforms.len() >= 64 {
            remote.waveforms.clear();
        }
        remote.waveforms.insert(memory_key, data.clone());
        Ok(data)
    }
}
