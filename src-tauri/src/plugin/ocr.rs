//! 离线文字识别（OCR）：调 Windows.Media.Ocr（WinRT），完全本地、免费、支持中文（需系统装了对应语言包）。
//! 输入 base64 图片，输出识别文本。不联网、不外发。

/// 对一张图片（base64，任意 image 可解码格式，内部按 PNG 喂给 WinRT 解码器）做 OCR。
/// lang 可选（如 "zh-Hans" / "en"），缺省用系统用户语言。
#[tauri::command]
pub async fn plugin_ocr(b64: String, lang: Option<String>) -> Result<String, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| format!("base64 解码失败: {e}"))?;
    // 统一转成 PNG（WinRT 解码器认 PNG）；超大图先降采样——WinRT OCR 单边上限约 10000px，
    // 超限会抛晦涩的 WinRT 错误；4000px 足够识别且更快。
    let png = {
        let mut img = image::load_from_memory(&bytes).map_err(|e| format!("图片解码失败: {e}"))?;
        let (w, h) = (img.width(), img.height());
        const CAP: u32 = 4000;
        if w > CAP || h > CAP {
            let k = CAP as f64 / w.max(h) as f64;
            img = img.resize(
                ((w as f64 * k) as u32).max(1),
                ((h as f64 * k) as u32).max(1),
                image::imageops::FilterType::Triangle,
            );
        }
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png)
            .map_err(|e| format!("PNG 编码失败: {e}"))?;
        out.into_inner()
    };
    tauri::async_runtime::spawn_blocking(move || ocr_png(&png, lang.as_deref()))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(windows)]
pub(crate) fn ocr_png(png: &[u8], lang: Option<&str>) -> Result<String, String> {
    use windows::core::HSTRING;
    use windows::Globalization::Language;
    use windows::Graphics::Imaging::BitmapDecoder;
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    // 该 blocking 线程需初始化 COM（MTA）才能跑 WinRT；已初始化返回 S_FALSE，忽略
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let e2s = |e: windows::core::Error| e.message().to_string();

    // PNG 字节 → 内存流
    let stream = InMemoryRandomAccessStream::new().map_err(e2s)?;
    let writer = DataWriter::CreateDataWriter(&stream).map_err(e2s)?;
    writer.WriteBytes(png).map_err(e2s)?;
    writer.StoreAsync().map_err(e2s)?.get().map_err(e2s)?;
    writer.FlushAsync().map_err(e2s)?.get().map_err(e2s)?;
    let _ = writer.DetachStream();
    stream.Seek(0).map_err(e2s)?;

    // 解码为 SoftwareBitmap
    let decoder = BitmapDecoder::CreateAsync(&stream).map_err(e2s)?.get().map_err(e2s)?;
    let bitmap = decoder.GetSoftwareBitmapAsync().map_err(e2s)?.get().map_err(e2s)?;

    // 建 OCR 引擎（指定语言或跟随系统用户语言）
    let engine = match lang {
        Some(l) if !l.is_empty() => {
            let language = Language::CreateLanguage(&HSTRING::from(l))
                .map_err(|_| format!("不支持的 OCR 语言：{l}"))?;
            OcrEngine::TryCreateFromLanguage(&language)
                .map_err(|_| format!("系统未安装 {l} 的 OCR 语言包"))?
        }
        _ => OcrEngine::TryCreateFromUserProfileLanguages()
            .map_err(|_| "系统无可用 OCR 语言（请在 Windows 设置里安装语言的手写/OCR 组件）".to_string())?,
    };

    let result = engine.RecognizeAsync(&bitmap).map_err(e2s)?.get().map_err(e2s)?;
    result.Text().map(|h| h.to_string()).map_err(e2s)
}

#[cfg(not(windows))]
fn ocr_png(_png: &[u8], _lang: Option<&str>) -> Result<String, String> {
    Err("OCR 仅在 Windows 上可用".to_string())
}
