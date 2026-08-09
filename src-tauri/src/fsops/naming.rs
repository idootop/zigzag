//! 产物文件名模板。
//!
//! 模板只决定**文件名那一段**，不含目录。目录由 [`crate::core::plan::dst_dir_for`]
//! 按镜像规则算出来，模板动不了它——输出树能整棵替代源目录是 ADR-019 §5 承诺过的
//! 事，一个能写 `/` 的模板就能把十万个文件拍平进一个目录，然后靠 `-1`、`-2` 消解
//! 到 999 为止。
//!
//! ## 三个占位符，不多
//!
//! | 占位符 | 展开成 | 例（`/照片/IMG_0001.HEIC` → AVIF） |
//! |---|---|---|
//! | `{name}` | 源文件主名 | `IMG_0001` |
//! | `{ext}` | 产物扩展名 | `avif` |
//! | `{srcext}` | 源扩展名（原样大小写） | `HEIC` |
//!
//! 默认模板 [`DEFAULT`] 就是 `{name}.{ext}`，与没有模板时的行为逐字节一致。
//!
//! `{srcext}` 不是凑数的：iPhone 导出常常同时留下 `IMG_0001.HEIC` 和
//! `IMG_0001.JPG`，两者的产物都叫 `IMG_0001.avif`，现在靠 `-1` 后缀消解——谁拿到
//! 后缀取决于认领顺序，重跑一次可能就换了人。写成 `{name}_{srcext}.{ext}` 则得到
//! `IMG_0001_HEIC.avif` 与 `IMG_0001_JPG.avif`，从源头上不撞。
//!
//! ## 为什么没有 `{w}x{h}`
//!
//! 原计划里有，实现前先去查了数据，三条都不成立：
//!
//! 1. **图片的宽高根本没落库。** 扫描期图片走 `scan::probe::probe_image`
//!    （`imagesize` 读文件头）拿到尺寸后直接喂给报告，既不写 `probe_cache`
//!    也没有 `items.width` 这一列。要在认领循环里补，就是每个文件再读一次头
//!    （冷缓存 136 us，十万文件 13.6 s），而认领循环是**串行**的——消解必须串行，
//!    见 [`crate::core::plan`]——这 13.6 s 全加在供给端。
//! 2. **就算读了也可能是错的。** `imagesize` 不解析 EXIF 朝向，而产物是把朝向
//!    烘焙进像素的（`platform::imageio::info` 的文档记了同一件事，D-133 就是为它立的）。
//!    旋转过的照片会得到一个宽高互换的名字，而且是**永久写在磁盘上**的错。
//! 3. **产物宽高要等编码完才真定**，文件名却必须在编码前定下来并占位。
//!
//! 一个可能骗人的文件名比没有这个占位符糟得多，所以不做。想在名字里记下这次压到
//! 多大，把数字当字面量写进模板即可：`{name}_1080.{ext}`。

use std::path::Path;

/// 默认模板：与「没有模板」时的行为完全一致。
pub const DEFAULT: &str = "{name}.{ext}";

/// 单段文件名的字节上限。
///
/// 实测（APFS，`/tmp`）：255 字节建得出来，256 字节报 `ENAMETOOLONG (63)`。
/// 注意是**字节**——一个中文名 85 个字就到顶了。
pub const NAME_MAX: usize = 255;

/// 认得的占位符。多一个少一个都要同步 [`validate`] 的错误文案。
const KEYS: [&str; 3] = ["name", "ext", "srcext"];

/// 校验模板。返回的错误是给用户看的中文，直接显示在设置界面上。
///
/// 模板来自输入框和手改的配置文件，都是不可信输入。这里宁可拦得严一点：
/// 一个写错的模板不会当场报错，它会安安静静地把十万个产物写成奇怪的名字。
pub fn validate(t: &str) -> Result<(), String> {
    if t.trim().is_empty() {
        return Err("模板不能为空".into());
    }
    if t.contains('/') {
        return Err("模板不能包含 /：它只决定文件名，目录由镜像规则决定".into());
    }

    // 逐个揪出 {...}，认不出的当场说清楚，而不是原样输出一个 `{nmae}`。
    let mut rest = t;
    let mut seen_name = false;
    while let Some(i) = rest.find('{') {
        let after = &rest[i + 1..];
        let Some(j) = after.find('}') else {
            return Err("模板里有没闭合的 {".into());
        };
        let key = &after[..j];
        if !KEYS.contains(&key) {
            return Err(format!("认不出占位符 {{{key}}}，可用的是 {{name}}、{{ext}}、{{srcext}}"));
        }
        seen_name |= key == "name";
        rest = &after[j + 1..];
    }
    if rest.contains('}') {
        return Err("模板里有多余的 }".into());
    }

    if !seen_name {
        return Err("模板必须包含 {name}，否则同一个目录里的文件会全部重名".into());
    }
    if !t.ends_with(".{ext}") {
        return Err("模板必须以 .{ext} 结尾，否则产物的扩展名对不上真实格式".into());
    }
    Ok(())
}

/// 按模板渲染一个文件名。
///
/// `target_ext` 是产物扩展名（不带点）。返回值保证：
/// - 不含 `/`——占位符展开出来的值也会被洗一遍；
/// - 不超过 [`NAME_MAX`] 字节，超了从主名尾部按 UTF-8 边界切；
/// - 以 `.target_ext` 结尾。
///
/// 模板非法时（配置文件被手改过、[`validate`] 没拦住的角落）退回「主名 + 目标
/// 扩展名」——产物的扩展名必须对，这一点没有商量余地。
pub fn render(t: &str, src: &Path, target_ext: &str) -> String {
    let body = t.strip_suffix(".{ext}").unwrap_or("{name}");
    // 洗掉 `/`。带 `/` 的模板 [`validate`] 会拦下，但配置文件是用户手改得到的，
    // 而这一层漏了就不是「名字难看」而是「产物写到别的目录去」。
    let stem = expand(body, src, target_ext).replace('/', "_");
    fit(&stem, &dot(target_ext))
}

/// `avif` → `.avif`，空扩展名 → 空串（不留一个孤零零的点）。
pub fn dot(ext: &str) -> String {
    if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    }
}

/// 展开占位符。认不出的原样留着——这条路只有非法模板才走得到，
/// 留个 `{nmae}` 在名字上，比悄悄吞掉更容易被发现。
fn expand(body: &str, src: &Path, target_ext: &str) -> String {
    let part =
        |v: Option<&std::ffi::OsStr>| v.map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut out = String::with_capacity(body.len() + 32);
    let mut rest = body;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(j) = after.find('}') else {
            out.push_str(&rest[i..]);
            return out;
        };
        match &after[..j] {
            "name" => out.push_str(&part(src.file_stem())),
            "srcext" => out.push_str(&part(src.extension())),
            "ext" => out.push_str(target_ext),
            other => {
                out.push('{');
                out.push_str(other);
                out.push('}');
            }
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    out
}

/// 把 `主名 + 尾巴` 收进 [`NAME_MAX`] 字节，**只切主名**。
///
/// 归档盘上真有 200 多字节的文件名（整句话当标题的截图、从网页另存的图片），
/// 模板再往上加几个字就越界了——越界的表现是写入直接失败，一个都写不出来。
/// 切的是主名的尾部：前缀比后缀有信息量。
///
/// 尾巴是「必须原样留住」的那一段——`.avif`，或者冲突消解时的 `-1.avif`。
/// 让尾巴参与截断会出一个隐蔽的死循环：名字顶到上限时 `-1` 自己被切掉，
/// 消解出来的候选名与原名一模一样，999 次全撞。
pub fn fit(stem: &str, tail: &str) -> String {
    let budget = NAME_MAX.saturating_sub(tail.len());
    let mut cut = stem.len().min(budget);
    while cut > 0 && !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{tail}", &stem[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn r(t: &str, src: &str) -> String {
        render(t, Path::new(src), "avif")
    }

    #[test]
    fn the_default_template_changes_nothing() {
        assert_eq!(r(DEFAULT, "/照片/IMG_0001.HEIC"), "IMG_0001.avif");
        // 多点名字只丢最后一段，和 `Path::with_extension` 一致。
        assert_eq!(r(DEFAULT, "/r/a.2020.01.jpg"), "a.2020.01.avif");
        // 没有扩展名的源，产物照样有。
        assert_eq!(r(DEFAULT, "/r/IMG_0001"), "IMG_0001.avif");
    }

    #[test]
    fn srcext_pulls_apart_the_iphone_pair() {
        // 不加区分时两者的产物同名，谁拿到 -1 后缀取决于认领顺序。
        assert_eq!(r("{name}_{srcext}.{ext}", "/i/IMG_0001.HEIC"), "IMG_0001_HEIC.avif");
        assert_eq!(r("{name}_{srcext}.{ext}", "/i/IMG_0001.JPG"), "IMG_0001_JPG.avif");
    }

    #[test]
    fn literals_are_kept_verbatim() {
        assert_eq!(r("{name}_1080.{ext}", "/i/a.jpg"), "a_1080.avif");
        assert_eq!(r("压缩_{name}.{ext}", "/i/照片.jpg"), "压缩_照片.avif");
    }

    #[test]
    fn a_hand_edited_slash_can_never_create_a_directory() {
        // validate 会拦下带 `/` 的模板，但配置文件在用户手上。渲染这一层漏了，
        // 产物就写到别的目录去了——那是数据安全问题，不是显示问题。
        assert_eq!(r("{name}/x.{ext}", "/i/a.jpg"), "a_x.avif");
    }

    #[test]
    fn a_long_name_is_cut_to_fit_the_filesystem() {
        // 实测 APFS：255 字节能建，256 报 ENAMETOOLONG。
        let long = "x".repeat(300);
        let got = r("{name}_压缩后.{ext}", &format!("/i/{long}.jpg"));
        assert_eq!(got.len(), NAME_MAX);
        assert!(got.ends_with(".avif"), "扩展名必须留住：{got}");
    }

    #[test]
    fn cutting_never_splits_a_utf8_character() {
        // 中文名一个字 3 字节，255 不是 3 的倍数，切点必然落在字符中间。
        let long = "照".repeat(200);
        let got = r("{name}.{ext}", &format!("/i/{long}.jpg"));
        assert!(got.len() <= NAME_MAX);
        assert!(std::str::from_utf8(got.as_bytes()).is_ok());
        assert!(got.ends_with(".avif"));
    }

    #[test]
    fn fit_leaves_short_names_alone() {
        assert_eq!(fit("a", ".avif"), "a.avif");
        assert_eq!(fit("a", ""), "a");
        assert_eq!(dot(""), "", "没有扩展名时不该多一个点");
    }

    #[test]
    fn the_tail_survives_even_when_the_name_is_already_at_the_limit() {
        // 冲突消解用的 `-1` 要是被截掉，候选名就等于原名，999 次全撞。
        let got = fit(&"x".repeat(NAME_MAX), "-1.avif");
        assert_eq!(got.len(), NAME_MAX);
        assert!(got.ends_with("-1.avif"), "{got}");
    }

    #[test]
    fn validate_accepts_what_the_ui_offers() {
        for t in [DEFAULT, "{name}_{srcext}.{ext}", "{name}_1080.{ext}", "归档_{name}.{ext}"] {
            assert!(validate(t).is_ok(), "{t}: {:?}", validate(t));
        }
    }

    #[test]
    fn validate_rejects_a_template_that_would_flatten_the_tree() {
        // `{name}/{name}.{ext}` 这种能建目录，也能靠 `../` 爬出输出根。
        assert!(validate("{name}/x.{ext}").is_err());
        assert!(validate("../{name}.{ext}").is_err());
    }

    #[test]
    fn validate_rejects_a_template_without_a_name() {
        // 整个目录只剩一个名字，其余全靠 -1、-2 消解，撞满 999 就开始丢文件。
        assert!(validate("压缩.{ext}").is_err());
    }

    #[test]
    fn validate_rejects_a_wrong_extension() {
        assert!(validate("{name}.{ext}.bak").is_err(), "扩展名必须对得上真实格式");
        assert!(validate("{name}.jpg").is_err(), "写死扩展名等于给 AVIF 套个 .jpg");
    }

    #[test]
    fn validate_names_the_typo_it_found() {
        let e = validate("{nmae}.{ext}").unwrap_err();
        assert!(e.contains("nmae"), "错误里要指出是哪个占位符：{e}");
        assert!(validate("{name.{ext}").is_err(), "没闭合的 {{");
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(validate("").is_err());
        assert!(validate("   ").is_err());
    }

    #[test]
    fn a_broken_template_still_produces_a_correct_extension() {
        // 配置文件被手改成非法值、又绕过了 sanitize 的角落：名字可以怪，
        // 扩展名不能错——错了整个应用都会把它当成另一种格式。
        assert_eq!(r("{name}.jpg", "/i/a.HEIC"), "a.avif");
    }
}
