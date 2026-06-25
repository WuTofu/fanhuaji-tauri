use crate::build_service_info;
use crate::epub;
use crate::{
    API_BASE, ApiResponse, ConvertEpubParams, ConvertFileParams, ConvertFileResult, EpubProgress,
    HttpClient, ServiceInfo, build_api_params, build_output_name, check_file_size,
    resolve_output_dir, validate_api_response,
};
use std::path::Path;
use tauri::Emitter;
use tauri_plugin_dialog::DialogExt;

#[tauri::command]
pub async fn get_service_info(client: tauri::State<'_, HttpClient>) -> Result<ServiceInfo, String> {
    let url = format!("{API_BASE}/service-info");
    let client = &client.0;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("NET_REQUEST_FAILED:{e}"))?;

    let info = resp
        .json()
        .await
        .map_err(|e| format!("RESPONSE_PARSE_FAILED:{e}"))?;

    build_service_info(info)
}

#[tauri::command]
pub async fn pick_save_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let path = app
        .dialog()
        .file()
        .set_title("選擇輸出資料夾")
        .blocking_pick_folder();

    Ok(path.map(|p| p.to_string()))
}

#[tauri::command]
pub async fn open_files_dialog(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let paths = app
        .dialog()
        .file()
        .add_filter(
            "支援檔案",
            &[
                "txt", "srt", "ass", "ssa", "lrc", "vtt", "sub", "sup", "csv", "tsv", "json",
                "xml", "html", "htm", "md", "epub",
            ],
        )
        .add_filter("所有檔案", &["*"])
        .set_title("開啟檔案")
        .blocking_pick_files();

    match paths {
        Some(files) => Ok(files.iter().map(|f| f.to_string()).collect()),
        None => Ok(vec![]),
    }
}

#[tauri::command]
pub async fn convert_file(
    client: tauri::State<'_, HttpClient>,
    params: ConvertFileParams,
) -> Result<ConvertFileResult, String> {
    let ConvertFileParams {
        input_path,
        converter,
        save_folder,
        naming,
        custom_suffix,
        pre_replace,
        post_replace,
        protect_replace,
        modules,
    } = params;

    // Canonicalize and validate input path
    let canonical = tokio::fs::canonicalize(&input_path)
        .await
        .map_err(|e| format!("INVALID_PATH:{e}"))?;

    // Check file size
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|e| format!("FILE_METADATA_FAILED:{e}"))?;
    check_file_size(metadata.len())?;

    // Read the file
    let content = tokio::fs::read_to_string(&canonical)
        .await
        .map_err(|e| format!("FILE_READ_FAILED:{e}"))?;

    // Build API params
    let params = build_api_params(
        &content,
        &converter,
        &pre_replace,
        &post_replace,
        &protect_replace,
        &modules,
    );

    // Call API
    let url = format!("{API_BASE}/convert");
    let resp = client
        .0
        .post(&url)
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("NET_REQUEST_FAILED:{e}"))?;

    let api: ApiResponse = resp
        .json()
        .await
        .map_err(|e| format!("RESPONSE_PARSE_FAILED:{e}"))?;

    let data = validate_api_response(api)?;

    // Determine output directory
    let input = Path::new(&input_path);
    let dir = resolve_output_dir(input, &save_folder)?;

    let output_name = build_output_name(input, &naming, &data.converter, &custom_suffix)?;

    // Build output path from canonical directory to prevent traversal
    let canonical_dir = tokio::fs::canonicalize(&dir)
        .await
        .map_err(|e| format!("OUTPUT_DIR_INVALID:{e}"))?;
    let output_path = canonical_dir.join(&output_name);

    // Write output
    tokio::fs::write(&output_path, &data.text)
        .await
        .map_err(|e| format!("FILE_WRITE_FAILED:{e}"))?;

    Ok(ConvertFileResult {
        output_name,
        output_path: output_path.to_string_lossy().into_owned(),
        warnings: None,
    })
}

/// Cap on characters returned to the UI for a preview, to keep payloads small.
const PREVIEW_CHAR_LIMIT: usize = 8000;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewResult {
    original: String,
    converted: String,
    truncated: bool,
}

/// Convert a file via the API and return the original + converted text (capped)
/// for an on-screen diff preview. Unlike `convert_file`, nothing is written to disk.
#[tauri::command]
pub async fn preview_convert(
    client: tauri::State<'_, HttpClient>,
    params: ConvertFileParams,
) -> Result<PreviewResult, String> {
    // Canonicalize, validate, and read the input — same guards as convert_file.
    let canonical = tokio::fs::canonicalize(&params.input_path)
        .await
        .map_err(|e| format!("INVALID_PATH:{e}"))?;
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|e| format!("FILE_METADATA_FAILED:{e}"))?;
    check_file_size(metadata.len())?;
    let content = tokio::fs::read_to_string(&canonical)
        .await
        .map_err(|e| format!("FILE_READ_FAILED:{e}"))?;

    let api_params = build_api_params(
        &content,
        &params.converter,
        &params.pre_replace,
        &params.post_replace,
        &params.protect_replace,
        &params.modules,
    );

    let url = format!("{API_BASE}/convert");
    let resp = client
        .0
        .post(&url)
        .form(&api_params)
        .send()
        .await
        .map_err(|e| format!("NET_REQUEST_FAILED:{e}"))?;

    let api: ApiResponse = resp
        .json()
        .await
        .map_err(|e| format!("RESPONSE_PARSE_FAILED:{e}"))?;

    let data = validate_api_response(api)?;

    let truncated = content.chars().count() > PREVIEW_CHAR_LIMIT
        || data.text.chars().count() > PREVIEW_CHAR_LIMIT;
    let original: String = content.chars().take(PREVIEW_CHAR_LIMIT).collect();
    let converted: String = data.text.chars().take(PREVIEW_CHAR_LIMIT).collect();

    Ok(PreviewResult {
        original,
        converted,
        truncated,
    })
}

/// Convert a single XML/XHTML file in the EPUB temp directory in-place.
/// Reads the file, extracts text nodes, calls the Fanhuaji API, replaces text
/// nodes with the converted result, and writes the file back.
///
/// Returns `Ok(())` on success (including when the file has no text nodes to
/// convert). Returns `Err` with a diagnostic code on any failure.
#[allow(clippy::too_many_arguments)]
async fn convert_epub_file(
    http: &reqwest::Client,
    url: &str,
    file_path: &std::path::Path,
    converter: &str,
    pre_replace: &str,
    post_replace: &str,
    protect_replace: &str,
    modules: &str,
) -> Result<(), String> {
    let xml = tokio::fs::read_to_string(file_path)
        .await
        .map_err(|e| format!("FILE_READ_FAILED:{e}"))?;

    let (text, count) = epub::extract_text(&xml)?;

    if count == 0 {
        return Ok(()); // Nothing to convert — not a failure
    }

    let api_params = build_api_params(
        &text,
        converter,
        pre_replace,
        post_replace,
        protect_replace,
        modules,
    );

    let resp = http
        .post(url)
        .form(&api_params)
        .send()
        .await
        .map_err(|e| format!("NET_REQUEST_FAILED:{e}"))?;

    let api: ApiResponse = resp
        .json()
        .await
        .map_err(|e| format!("RESPONSE_PARSE_FAILED:{e}"))?;

    if api.code != 0 {
        return Err(format!("API_ERROR:{}", api.code));
    }

    let data = api.data.ok_or_else(|| "API_NO_DATA".to_string())?;

    let new_xml = epub::replace_text(&xml, &data.text)?;

    tokio::fs::write(file_path, new_xml)
        .await
        .map_err(|e| format!("FILE_WRITE_FAILED:{e}"))
}

#[tauri::command]
pub async fn convert_epub(
    app: tauri::AppHandle,
    client: tauri::State<'_, HttpClient>,
    params: ConvertEpubParams,
) -> Result<ConvertFileResult, String> {
    let ConvertEpubParams {
        file_id,
        input_path,
        converter,
        save_folder,
        naming,
        custom_suffix,
        pre_replace,
        post_replace,
        protect_replace,
        modules,
    } = params;

    let canonical = tokio::fs::canonicalize(&input_path)
        .await
        .map_err(|e| format!("INVALID_PATH:{e}"))?;

    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|e| format!("FILE_METADATA_FAILED:{e}"))?;
    check_file_size(metadata.len())?;

    // Extract EPUB
    let canonical_clone = canonical.clone();
    let (temp_dir, content_files, metadata_files) =
        tokio::task::spawn_blocking(move || epub::extract_epub(&canonical_clone))
            .await
            .map_err(|e| format!("EPUB_EXTRACT_FAILED:{e}"))??;

    let chapter_total = content_files.len() + metadata_files.len();
    let url = format!("{API_BASE}/convert");
    let mut failed_chapters: usize = 0;

    // Convert all files — chapter bodies first, then .opf/.ncx metadata.
    // Processing them in a single loop ensures any future policy change
    // (retry logic, delay tuning, error handling) applies uniformly.
    for (i, file) in content_files.iter().chain(metadata_files.iter()).enumerate() {
        let chapter_name = epub::chapter_display_name(&file.relative_path);

        let _ = app.emit(
            "epub-progress",
            EpubProgress {
                file_id: file_id.clone(),
                chapter_index: i + 1,
                chapter_total,
                chapter_name: chapter_name.clone(),
            },
        );

        let file_path = temp_dir.path().join(&file.relative_path);
        if let Err(e) = convert_epub_file(
            &client.0,
            &url,
            &file_path,
            &converter,
            &pre_replace,
            &post_replace,
            &protect_replace,
            &modules,
        )
        .await
        {
            eprintln!("EPUB_CHAPTER_FAILED:{chapter_name}:{e}");
            failed_chapters += 1;
        }

        // Small delay between API calls
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Determine output path
    let input = Path::new(&input_path);
    let dir = resolve_output_dir(input, &save_folder)?;

    let output_name = build_output_name(input, &naming, &converter, &custom_suffix)?;
    let canonical_dir = tokio::fs::canonicalize(&dir)
        .await
        .map_err(|e| format!("OUTPUT_DIR_INVALID:{e}"))?;
    let output_path = canonical_dir.join(&output_name);

    // Repack EPUB
    let temp_path = temp_dir.path().to_path_buf();
    let out_path = output_path.clone();
    tokio::task::spawn_blocking(move || epub::repack_epub(&temp_path, &out_path))
        .await
        .map_err(|e| format!("EPUB_REPACK_FAILED:{e}"))??;

    let warnings = if failed_chapters == 0 {
        None
    } else {
        Some(format!(
            "EPUB_PARTIAL_FAILED:{failed_chapters}/{chapter_total}"
        ))
    };

    Ok(ConvertFileResult {
        output_name,
        output_path: output_path.to_string_lossy().into_owned(),
        warnings,
    })
}

pub fn run() {
    tauri::Builder::default()
        .manage(HttpClient(
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("無法建立 HTTP 客戶端"),
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            #[cfg(desktop)]
            app.handle()
                .plugin(tauri_plugin_updater::Builder::new().build())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_service_info,
            pick_save_folder,
            open_files_dialog,
            convert_file,
            convert_epub,
            preview_convert,
        ])
        .run(tauri::generate_context!())
        .expect("啟動應用程式時發生錯誤");
}
