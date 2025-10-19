#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

use anyhow::{anyhow, Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder, ImageFormat, ImageReader};
use slint::{ComponentHandle, SharedString};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::thread;
use walkdir::WalkDir;

fn main() -> Result<()> {
    let app = AppWindow::new()?;

    let ui_weak = app.as_weak();

    app.on_pick_folder({
        let ui_weak = ui_weak.clone();
        move || {
            if let Some(selected) = rfd::FileDialog::new().pick_folder() {
                if let Some(ui) = ui_weak.upgrade() {
                    let path_text: SharedString = selected.display().to_string().into();
                    ui.set_selected_folder(path_text.clone());
                    ui.set_status_text(format!("已选择文件夹: {}", path_text).into());
                }
            }
        }
    });

    app.on_start_compress({
        let ui_weak = ui_weak.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };

            if ui.get_busy() {
                return;
            }

            let folder: String = ui.get_selected_folder().as_str().into();
            if folder.is_empty() {
                ui.set_status_text("请先选择文件夹".into());
                return;
            }

            let quality = ui.get_jpeg_quality().round().clamp(1.0, 100.0) as u8;

            ui.set_busy(true);
            ui.set_status_text("正在扫描图像文件...".into());
            ui.set_log_text("".into());
            ui.set_processed_files(0);
            ui.set_total_files(0);
            ui.set_progress(0.0);

            let ui_weak_for_thread = ui_weak.clone();
            thread::spawn(move || {
                if let Err(err) = process_folder(ui_weak_for_thread.clone(), folder, quality) {
                    let message = format!("压缩失败: {err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_for_thread.upgrade() {
                            let mut log = ui.get_log_text().to_string();
                            if !log.is_empty() {
                                log.push('\n');
                            }
                            log.push_str(&message);
                            ui.set_busy(false);
                            ui.set_status_text(message.clone().into());
                            ui.set_log_text(log.into());
                        }
                    });
                }
            });
        }
    });

    app.run()?;
    Ok(())
}

fn process_folder(ui_weak: slint::Weak<AppWindow>, folder: String, quality: u8) -> Result<()> {
    let folder_path = PathBuf::from(&folder);
    if !folder_path.exists() {
        return Err(anyhow!("路径不存在: {folder}"));
    }
    if !folder_path.is_dir() {
        return Err(anyhow!("选择的路径不是文件夹: {folder}"));
    }

    let mut files: Vec<PathBuf> = Vec::new();
    let mut log_builder = String::new();

    for entry in WalkDir::new(&folder_path).into_iter() {
        match entry {
            Ok(e) => {
                if e.file_type().is_file() && is_supported_image(e.path()) {
                    files.push(e.into_path());
                }
            }
            Err(err) => {
                log_builder.push_str(&format!("遍历时出错: {err}\n"));
            }
        }
    }

    let total = files.len();
    {
        let status = if total > 0 {
            format!("找到 {total} 个图像文件")
        } else {
            "未找到可压缩的图像".to_string()
        };
        let log_snapshot = log_builder.clone();
        let ui_weak = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_total_files(total as i32);
                ui.set_processed_files(0);
                ui.set_progress(0.0);
                ui.set_log_text(log_snapshot.into());
                ui.set_status_text(status.into());
            }
        });
    }

    if total == 0 {
        let ui_weak = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_busy(false);
            }
        });
        return Ok(());
    }

    let mut total_saved: i64 = 0;

    for (index, path) in files.iter().enumerate() {
        let display_path = path.display().to_string();
        match compress_image(path, quality) {
            Ok(stats) => {
                let saved = stats.original_size.saturating_sub(stats.new_size) as i64;
                total_saved += saved;
                log_builder.push_str(&format!(
                    "✔ {} | {:.2} KB → {:.2} KB (节省 {:.2}%)\n",
                    display_path,
                    bytes_to_kb(stats.original_size),
                    bytes_to_kb(stats.new_size),
                    savings_percent(stats.original_size, stats.new_size)
                ));
            }
            Err(err) => {
                log_builder.push_str(&format!("✖ {} | 失败: {}\n", display_path, err));
            }
        }

        let processed = index + 1;
        let progress = processed as f32 / total as f32;
        let log_snapshot = log_builder.clone();
        let status = format!("正在处理: {} ({}/{})", display_path, processed, total);
        let ui_weak = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_processed_files(processed as i32);
                ui.set_progress(progress);
                ui.set_log_text(log_snapshot.into());
                ui.set_status_text(status.clone().into());
            }
        });
    }

    let final_status = if total_saved >= 0 {
        format!(
            "完成: 共处理 {total} 个图像，累计节省 {:.2} MB",
            bytes_to_mb(total_saved as u64)
        )
    } else {
        format!(
            "完成: 共处理 {total} 个图像，文件总体增大 {:.2} MB",
            bytes_to_mb((-total_saved) as u64)
        )
    };
    let log_snapshot = log_builder.clone();
    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_status_text(final_status.into());
            ui.set_log_text(log_snapshot.into());
            ui.set_progress(1.0);
            ui.set_busy(false);
        }
    });

    Ok(())
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" | "png" => true,
            _ => false,
        })
        .unwrap_or(false)
}

fn compress_image(path: &Path, quality: u8) -> Result<CompressionStats> {
    let mut reader = ImageReader::open(path)
        .with_context(|| format!("无法打开图像: {}", path.display()))?;
    reader.no_limits();
    reader = reader
        .with_guessed_format()
        .with_context(|| format!("无法识别图像格式: {}", path.display()))?;

    let format = reader
        .format()
        .ok_or_else(|| anyhow!("无法确定图像格式: {}", path.display()))?;

    let image = reader
        .decode()
        .with_context(|| format!("无法解码图像: {}", path.display()))?;

    let mut cursor = Cursor::new(Vec::new());
    match format {
        ImageFormat::Jpeg => {
            let mut encoder = JpegEncoder::new_with_quality(&mut cursor, quality.max(1));
            encoder
                .encode_image(&image)
                .with_context(|| format!("无法重新编码图像: {}", path.display()))?;
        }
        ImageFormat::Png => {
            let rgba = image.to_rgba8();
            let (width, height) = rgba.dimensions();
            let mut encoder = PngEncoder::new_with_quality(
                &mut cursor,
                CompressionType::Best,
                FilterType::Adaptive,
            );
            encoder
                .write_image(
                    rgba.as_raw(),
                    width,
                    height,
                    ExtendedColorType::Rgba8,
                )
                .with_context(|| format!("无法重新编码图像: {}", path.display()))?;
        }
        other => {
            return Err(anyhow!("暂不支持重新编码 {:?} 格式", other));
        }
    }
    let buffer = cursor.into_inner();

    let original_size = fs::metadata(path)
        .with_context(|| format!("无法读取原文件大小: {}", path.display()))?
        .len();

    fs::write(path, &buffer)
        .with_context(|| format!("无法写回压缩结果: {}", path.display()))?;

    let new_size = buffer.len() as u64;
    Ok(CompressionStats {
        original_size,
        new_size,
    })
}

fn bytes_to_kb(bytes: u64) -> f64 {
    bytes as f64 / 1024.0
}

fn bytes_to_mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn savings_percent(before: u64, after: u64) -> f64 {
    if before == 0 {
        0.0
    } else {
        100.0 * (before as f64 - after as f64) / before as f64
    }
}

struct CompressionStats {
    original_size: u64,
    new_size: u64,
}
