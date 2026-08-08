//! 音频编码器封装：拼参数。选路（重编 / 只换容器 / 跳过）在 `core/policy`。
//!
//! 目标格式是 AAC-LC 装在 `.m4a` 里（D-18）。不用 Opus 是因为归档盘的意义在于
//! **随手能打开**：AAC-LC + m4a 在 Finder 预览、QuickTime、iPhone、车机上一路通吃，
//! Opus 在这条链上处处要额外解码器。
//!
//! ## 唯一一个反直觉的地方：封面必须显式映射
//!
//! §5.2 原来的命令是 `-map 0:a:0`，只映射音频流——于是**封面被静默丢掉**。
//! 实测一首带封面的 192k MP3，按原命令转出来的 m4a 里没有任何图片流；
//! 补上封面映射后产物 +789 B，Finder 与音乐 app 里封面回来了（D-70）。
//!
//! 这条不是锦上添花：选 m4a 而不是 Opus 的理由就是「在 Apple 生态里体验完整」，
//! 而丢封面恰恰破坏的就是这个理由。
//!
//! 封面流要**按索引显式指定**，不能写 `-map 0:v?`：`.mka` 这类容器里可能真躺着
//! 一条视频轨，那样会把整段视频拷进音频文件。所以由调用方从 ffprobe 结果里
//! 找出 `disposition.attached_pic=1` 的那一条，把索引交进来。

use std::path::Path;

use serde_json::Value;

use crate::config::Profile;

/// AAC-LC 编码器。
///
/// AudioToolbox 的实现，在 Apple Silicon 上质量与速度都优于 ffmpeg 内建的 `aac`。
/// 随包 ffmpeg 9.0 固定带它，不做运行期探测（D-68）。
pub fn aac_encoder() -> &'static str {
    "aac_at"
}

/// 输出容器扩展名。音频只有这一个目标格式，没有 mp4/mkv 那种分支。
pub const EXT: &str = "m4a";

/// 编码要用到的源信息。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Source {
    pub duration_us: u64,
    /// 主音频流的 `codec_name`。
    pub codec: Option<String>,
    /// 封面图所在的流索引（`disposition.attached_pic=1`）。
    pub cover: Option<u32>,
}

impl Source {
    pub fn from_probe(json: &Value) -> Self {
        let mut s = Self::default();
        let streams = json.get("streams").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);

        s.duration_us = json
            .get("format")
            .and_then(|f| f.get("duration"))
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|d| d.is_finite() && *d > 0.0)
            .map(|d| (d * 1_000_000.0) as u64)
            .unwrap_or(0);

        for st in streams {
            let idx = st.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
            let cover =
                st.get("disposition").and_then(|d| d.get("attached_pic")).and_then(Value::as_u64) == Some(1);
            match st.get("codec_type").and_then(Value::as_str) {
                Some("audio") if s.codec.is_none() => {
                    s.codec = st.get("codec_name").and_then(Value::as_str).map(str::to_string);
                }
                Some("video") if cover && s.cover.is_none() => s.cover = Some(idx),
                _ => {}
            }
        }
        s
    }
}

/// 这次要干什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// 重新编码成 AAC-LC。
    Encode,
    /// 源已经是 AAC，只换容器（`-c:a copy`），不重新编码。
    ///
    /// 有损编码每转一次都掉一层质量，而换容器是零损耗的位流搬运。实测
    /// 裸 `.aac` 327302 → 325502 B、`.mka` 328093 → 325682 B，内容一字未改。
    Remux,
}

impl Route {
    /// AAC 源在 `copy_if_aac` 打开时只换容器。
    pub fn pick(s: &Source, cfg: &Profile) -> Self {
        Self::for_codec(s.codec.as_deref(), cfg)
    }

    /// 只看编码名的版本。
    ///
    /// 扫描期和预估期手上只有 ffprobe 的 `codec_name`，没有完整 [`Source`]，但它们
    /// **必须和管线选出同一条路**：跳过规则与体积预估都是按「重编成 128k」算的，
    /// 套到一个实际只会换容器的文件上，就会承诺一份根本不会发生的收益。
    ///
    /// `aac_latm` 是另一种封装语法，同属 AAC-LC 位流，一样能直接搬进 m4a。
    pub fn for_codec(codec: Option<&str>, cfg: &Profile) -> Self {
        let is_aac = matches!(codec, Some("aac") | Some("aac_latm"));
        if cfg.audio.copy_if_aac && is_aac {
            Route::Remux
        } else {
            Route::Encode
        }
    }
}

/// 拼出完整的 ffmpeg 参数向量。
///
/// 不含 `-nostdin` / `-loglevel` / `-progress`，那些由 [`super::ffmpeg::run_with_progress`] 加。
pub fn args(src: &Path, out: &Path, s: &Source, cfg: &Profile, route: Route) -> Vec<String> {
    let t = |v: &str| v.to_string();
    let mut a = vec![t("-y"), t("-i"), src.to_string_lossy().into_owned()];

    // 只要第一条音频轨。歌曲/播客/语音备忘录不存在多音轨语义，
    // 而 `.mka` 里那种多轨往往是同一内容的不同语言配音，全留会把体积翻倍。
    a.extend([t("-map"), t("0:a:0")]);
    match route {
        Route::Encode => {
            a.extend([t("-c:a"), t(aac_encoder())]);
            a.extend([t("-b:a"), format!("{}k", cfg.audio.bitrate_kbps)]);
        }
        Route::Remux => a.extend([t("-c:a"), t("copy")]),
    }

    // 封面：显式按索引映射，并把 disposition 标回 attached_pic，
    // 否则播放器会把它当成一条 1 帧的视频轨。
    if let Some(i) = s.cover {
        a.extend([t("-map"), format!("0:{i}")]);
        a.extend([t("-c:v"), t("copy")]);
        a.extend([t("-disposition:v:0"), t("attached_pic")]);
    }

    // ID3 / Vorbis comment 里的标题、艺人、专辑都靠这一条搬过去。
    a.extend([t("-map_metadata"), t("0")]);
    a.extend([t("-movflags"), t("+faststart")]);
    // 临时文件叫 `.xxx.tmp`，ffmpeg 靠扩展名猜不出容器。
    a.extend([t("-f"), t("ipod")]);
    a.push(out.to_string_lossy().into_owned());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(a: &[String]) -> String {
        a.join(" ")
    }

    fn plain() -> Source {
        Source { duration_us: 120_000_000, codec: Some("mp3".into()), cover: None }
    }

    #[test]
    fn encodes_to_aac_lc_at_the_configured_bitrate() {
        let a = joined(&args(Path::new("i.mp3"), Path::new("o.tmp"), &plain(), &Profile::default(), Route::Encode));
        assert!(a.contains("-c:a aac_at"), "{a}");
        assert!(a.contains("-b:a 128k"), "{a}");
        assert!(a.contains("-map_metadata 0"), "标题/艺人/专辑靠这一条: {a}");
        assert!(a.contains("-f ipod"), "临时文件没有扩展名，必须显式指定容器: {a}");
    }

    #[test]
    fn cover_art_survives() {
        // D-70：只写 -map 0:a:0 会把封面静默丢掉，而「在 Apple 生态里体验完整」
        // 正是选 m4a 而不是 Opus 的全部理由。
        let s = Source { cover: Some(1), ..plain() };
        let a = joined(&args(Path::new("i.mp3"), Path::new("o.tmp"), &s, &Profile::default(), Route::Encode));
        assert!(a.contains("-map 0:1"), "{a}");
        assert!(a.contains("-c:v copy"), "封面不该被重新编码: {a}");
        assert!(a.contains("-disposition:v:0 attached_pic"), "不标 disposition 会被当成视频轨: {a}");
    }

    #[test]
    fn a_file_without_cover_art_gets_no_video_mapping() {
        let a = joined(&args(Path::new("i.mp3"), Path::new("o.tmp"), &plain(), &Profile::default(), Route::Encode));
        assert!(!a.contains("-c:v"), "没有封面还映射视频流会让 ffmpeg 报错: {a}");
    }

    #[test]
    fn an_aac_source_is_remuxed_not_re_encoded() {
        // 有损编码每转一次掉一层质量，换容器是零损耗的。
        for codec in ["aac", "aac_latm"] {
            let s = Source { codec: Some(codec.into()), ..plain() };
            assert_eq!(Route::pick(&s, &Profile::default()), Route::Remux, "{codec}");
        }
        let s = Source { codec: Some("aac".into()), ..plain() };
        let a = joined(&args(Path::new("i.aac"), Path::new("o.tmp"), &s, &Profile::default(), Route::Remux));
        assert!(a.contains("-c:a copy"), "{a}");
        assert!(!a.contains("-b:a"), "copy 模式下指定码率没有意义: {a}");
    }

    #[test]
    fn other_codecs_are_encoded() {
        for codec in ["mp3", "flac", "alac", "vorbis", "opus", "pcm_s16le"] {
            let s = Source { codec: Some(codec.into()), ..plain() };
            assert_eq!(Route::pick(&s, &Profile::default()), Route::Encode, "{codec}");
        }
    }

    #[test]
    fn remux_can_be_turned_off() {
        let mut cfg = Profile::default();
        cfg.audio.copy_if_aac = false;
        let s = Source { codec: Some("aac".into()), ..plain() };
        assert_eq!(Route::pick(&s, &cfg), Route::Encode);
    }

    #[test]
    fn parses_a_real_ffprobe_payload() {
        let json: Value = serde_json::from_str(
            r#"{"streams":[
                 {"index":0,"codec_type":"audio","codec_name":"mp3"},
                 {"index":1,"codec_type":"video","codec_name":"mjpeg",
                  "disposition":{"attached_pic":1}}],
               "format":{"duration":"120.5"}}"#,
        )
        .unwrap();
        let s = Source::from_probe(&json);
        assert_eq!(s.codec.as_deref(), Some("mp3"));
        assert_eq!(s.cover, Some(1));
        assert_eq!(s.duration_us, 120_500_000);
    }

    #[test]
    fn a_real_video_track_is_not_mistaken_for_cover_art() {
        // `.mka` 里可能真躺着一条视频轨。当成封面拷过去等于把整段视频塞进 m4a。
        let json: Value = serde_json::from_str(
            r#"{"streams":[
                 {"index":0,"codec_type":"audio","codec_name":"flac"},
                 {"index":1,"codec_type":"video","codec_name":"h264",
                  "disposition":{"attached_pic":0}}],
               "format":{"duration":"10"}}"#,
        )
        .unwrap();
        assert_eq!(Source::from_probe(&json).cover, None);
    }

    #[test]
    fn a_broken_payload_yields_defaults_instead_of_panicking() {
        for raw in ["{}", r#"{"streams":[]}"#, r#"{"streams":[{"codec_type":"audio"}]}"#] {
            let s = Source::from_probe(&serde_json::from_str(raw).unwrap());
            assert_eq!(s.cover, None);
            assert_eq!(Route::pick(&s, &Profile::default()), Route::Encode);
        }
    }
}
