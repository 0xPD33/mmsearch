//! EmbeddingGemma 2 through llama.cpp and mtmd, over the raw C API.

use anyhow::{bail, ensure, Context, Result};
use llama_cpp_sys_2 as sys;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

/// The model attends in both directions, so llama.cpp must get a whole prompt in one ubatch.
pub const N_CTX: u32 = 8192;
const IMAGE_TOKENS: i32 = 280;
/// Text inputs that one `llama_process` call can pool separately.
const MAX_SEQ: u32 = 16;
/// Packed sequences still pay attention over the whole batch, so a small pack is faster.
/// Measured on an RTX 5060 Ti: 1024 tokens gave 51 chunks/s, 8192 gave 30.
const PACK_TOKENS: usize = 1024;

/// The model id that the store records; a change rebuilds the index.
pub const MODEL_ID: &str = "embeddinggemma-2-Q8_0";

/// One media item for [`Embedder::embed`].
#[derive(Clone)]
pub enum Media {
    /// A file that mtmd decodes: jpg, png, bmp, gif, wav, mp3, flac.
    File(PathBuf),
    /// Packed 8-bit RGB pixels.
    Rgb {
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    /// Mono f32 samples at 16 kHz.
    Audio(Vec<f32>),
}

impl Media {
    pub fn from_image(img: &image::RgbImage) -> Media {
        Media::Rgb {
            width: img.width(),
            height: img.height(),
            data: img.as_raw().clone(),
        }
    }
}

pub struct Embedder {
    model: *mut sys::llama_model,
    ctx: *mut sys::llama_context,
    mctx: *mut sys::mtmd_context,
}

impl Embedder {
    pub fn load(model_path: &Path, mmproj_path: &Path) -> Result<Self> {
        let model_c = c_path(model_path)?;
        let mmproj_c = c_path(mmproj_path)?;
        // Drop frees whatever is non-null if a later step fails.
        let mut this = Self {
            model: ptr::null_mut(),
            ctx: ptr::null_mut(),
            mctx: ptr::null_mut(),
        };
        unsafe {
            sys::llama_log_set(Some(log), ptr::null_mut());
            sys::mtmd_helper_log_set(Some(log), ptr::null_mut());
            sys::llama_backend_init();

            let mut mp = sys::llama_model_default_params();
            mp.n_gpu_layers = 999;
            this.model = sys::llama_model_load_from_file(model_c.as_ptr(), mp);
            ensure!(
                !this.model.is_null(),
                "cannot load {}",
                model_path.display()
            );

            let mut cp = sys::llama_context_default_params();
            cp.embeddings = true;
            cp.pooling_type = sys::LLAMA_POOLING_TYPE_MEAN;
            cp.n_ctx = N_CTX;
            cp.n_batch = N_CTX;
            cp.n_ubatch = N_CTX;
            cp.n_seq_max = MAX_SEQ;
            // Without a unified cache, each sequence gets only N_CTX / MAX_SEQ tokens.
            cp.kv_unified = true;
            this.ctx = sys::llama_init_from_model(this.model, cp);
            ensure!(!this.ctx.is_null(), "cannot create the llama context");

            let mut mparams = sys::mtmd_context_params_default();
            mparams.use_gpu = true;
            mparams.print_timings = false;
            mparams.warmup = false;
            // The HF processor scales every image to fill max_soft_tokens. llama.cpp only
            // downscales, so a small image gets fewer tokens and a different vector.
            mparams.image_min_tokens = IMAGE_TOKENS;
            mparams.image_max_tokens = IMAGE_TOKENS;
            this.mctx = sys::mtmd_init_from_file(mmproj_c.as_ptr(), this.model, mparams);
            ensure!(
                !this.mctx.is_null(),
                "cannot load {}",
                mmproj_path.display()
            );
        }
        Ok(this)
    }

    /// Embeds one input into a unit vector. `prompt` holds one [`marker`] per item in `media`, in order.
    pub fn embed(&mut self, prompt: &str, media: &[Media]) -> Result<Vec<f32>> {
        let bitmaps = media
            .iter()
            .map(|m| self.bitmap(m))
            .collect::<Result<Vec<_>>>()?;
        let bitmap_ptrs: Vec<_> = bitmaps
            .iter()
            .map(|b| b.0 as *const sys::mtmd_bitmap)
            .collect();
        let prompt_c = CString::new(prompt)?;

        unsafe {
            let chunks = Chunks(sys::mtmd_input_chunks_init());
            let text = sys::mtmd_input_text {
                text: prompt_c.as_ptr(),
                text_len: prompt.len(),
                add_special: true,
                // File and query text must not inject control tokens such as <eos>. mtmd parses
                // its own media boundary tokens as special in any case.
                parse_special: false,
            };
            let rc = sys::mtmd_tokenize(
                self.mctx,
                chunks.0,
                &text,
                bitmap_ptrs.as_ptr(),
                bitmap_ptrs.len(),
            );
            ensure!(rc == 0, "mtmd_tokenize failed with {rc}");

            let n_embd = sys::llama_model_n_embd_inp(self.model) as usize;
            let batch = Batch(sys::llama_batch_ext_init(self.ctx));
            let mut pos: sys::llama_pos = 0;

            for i in 0..sys::mtmd_input_chunks_size(chunks.0) {
                let chunk = sys::mtmd_input_chunks_get(chunks.0, i);
                if sys::mtmd_input_chunk_get_type(chunk) == sys::MTMD_INPUT_CHUNK_TYPE_TEXT {
                    let mut n = 0;
                    let tokens = sys::mtmd_input_chunk_get_tokens_text(chunk, &mut n);
                    for &token in std::slice::from_raw_parts(tokens, n) {
                        batch.push(sys::llama_batch_ext_add_token(batch.0, 0, token), pos)?;
                        pos += 1;
                    }
                } else {
                    ensure!(
                        sys::mtmd_encode_chunk(self.mctx, chunk) == 0,
                        "media encode failed"
                    );
                    let n = sys::mtmd_input_chunk_get_n_tokens(chunk);
                    // The batch copies each row, so the mtmd output buffer can be reused.
                    let rows = std::slice::from_raw_parts(
                        sys::mtmd_get_output_embd(self.mctx),
                        n * n_embd,
                    );
                    for row in rows.chunks_exact(n_embd) {
                        let embd = sys::llama_embd {
                            data: row.as_ptr(),
                            n_rows: 1,
                            n_embd,
                        };
                        batch.push(sys::llama_batch_ext_add_embd(batch.0, 0, embd), pos)?;
                        pos += 1;
                    }
                }
            }
            self.process(&batch)?;
            self.pooled(0)
        }
    }

    /// Embeds text-only inputs, several per `llama_process` call. Each input is cut to fit the context.
    pub fn embed_texts(&mut self, prompts: &[String]) -> Result<Vec<Vec<f32>>> {
        let token_lists = prompts
            .iter()
            .map(|p| self.tokenize(p))
            .collect::<Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(prompts.len());
        let mut start = 0;
        while start < token_lists.len() {
            // Pack inputs until the context or the sequence limit is full.
            let mut end = start;
            let mut total = 0;
            while end < token_lists.len()
                && end - start < MAX_SEQ as usize
                && (end == start || total + token_lists[end].len() <= PACK_TOKENS)
            {
                total += token_lists[end].len();
                end += 1;
            }
            unsafe {
                let batch = Batch(sys::llama_batch_ext_init(self.ctx));
                for (seq, tokens) in token_lists[start..end].iter().enumerate() {
                    for (pos, &t) in tokens.iter().enumerate() {
                        batch.push(
                            sys::llama_batch_ext_add_token(batch.0, seq as i32, t),
                            pos as i32,
                        )?;
                    }
                }
                self.process(&batch)?;
                for seq in 0..end - start {
                    out.push(self.pooled(seq as i32)?);
                }
            }
            start = end;
        }
        Ok(out)
    }

    fn tokenize(&self, text: &str) -> Result<Vec<sys::llama_token>> {
        unsafe {
            let vocab = sys::llama_model_get_vocab(self.model);
            let mut tokens = vec![0; text.len() + 8];
            let mut n = sys::llama_tokenize(
                vocab,
                text.as_ptr().cast(),
                text.len() as i32,
                tokens.as_mut_ptr(),
                tokens.len() as i32,
                true,
                false,
            );
            if n < 0 {
                tokens.resize((-n) as usize, 0);
                n = sys::llama_tokenize(
                    vocab,
                    text.as_ptr().cast(),
                    text.len() as i32,
                    tokens.as_mut_ptr(),
                    tokens.len() as i32,
                    true,
                    false,
                );
            }
            ensure!(n >= 0, "tokenize failed");
            tokens.truncate(n as usize);
            // ponytail: plain cut at the context size; callers chunk long text well below it.
            tokens.truncate(N_CTX as usize);
            Ok(tokens)
        }
    }

    unsafe fn process(&self, batch: &Batch) -> Result<()> {
        let mem = sys::llama_get_memory(self.ctx);
        if !mem.is_null() {
            sys::llama_memory_clear(mem, true);
        }
        let rc = sys::llama_process(self.ctx, sys::LLAMA_PROCESS_TYPE_DECODE, batch.0);
        ensure!(rc == 0, "llama_process failed with {rc}");
        Ok(())
    }

    unsafe fn pooled(&self, seq: i32) -> Result<Vec<f32>> {
        let out = sys::llama_get_embeddings_seq(self.ctx, seq);
        ensure!(!out.is_null(), "no pooled embedding for sequence {seq}");
        let n_out = sys::llama_model_n_embd_out(self.model) as usize;
        let mut v = std::slice::from_raw_parts(out, n_out).to_vec();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        ensure!(norm.is_finite() && norm > 0.0, "embedding norm is {norm}");
        v.iter_mut().for_each(|x| *x /= norm);
        Ok(v)
    }

    fn bitmap(&self, media: &Media) -> Result<Bitmap> {
        unsafe {
            let bitmap = match media {
                Media::File(path) => {
                    let path_c = c_path(path)?;
                    let w = sys::mtmd_helper_bitmap_init_from_file(
                        self.mctx,
                        path_c.as_ptr(),
                        false,
                        sys::mtmd_helper_init_opt_default(),
                    );
                    if !w.video_ctx.is_null() {
                        sys::mtmd_helper_video_free(w.video_ctx);
                    }
                    if w.bitmap.is_null() {
                        bail!("cannot decode {}", path.display());
                    }
                    w.bitmap
                }
                Media::Rgb {
                    width,
                    height,
                    data,
                } => {
                    ensure!(
                        data.len() == (*width as usize) * (*height as usize) * 3,
                        "RGB buffer size does not match {width}x{height}"
                    );
                    sys::mtmd_bitmap_init(*width, *height, data.as_ptr())
                }
                Media::Audio(samples) => {
                    sys::mtmd_bitmap_init_from_audio(samples.len(), samples.as_ptr())
                }
            };
            ensure!(!bitmap.is_null(), "cannot create the media bitmap");
            Ok(Bitmap(bitmap))
        }
    }
}

impl Drop for Embedder {
    fn drop(&mut self) {
        unsafe {
            if !self.mctx.is_null() {
                sys::mtmd_free(self.mctx);
            }
            if !self.ctx.is_null() {
                sys::llama_free(self.ctx);
            }
            if !self.model.is_null() {
                sys::llama_model_free(self.model);
            }
        }
    }
}

/// Forwards llama.cpp warnings and errors to tracing and drops its info chatter.
unsafe extern "C" fn log(
    level: sys::ggml_log_level,
    text: *const std::os::raw::c_char,
    _: *mut std::os::raw::c_void,
) {
    if level != sys::GGML_LOG_LEVEL_WARN && level != sys::GGML_LOG_LEVEL_ERROR {
        return;
    }
    let msg = unsafe { CStr::from_ptr(text) }.to_string_lossy();
    let msg = msg.trim_end();
    if level == sys::GGML_LOG_LEVEL_ERROR {
        tracing::error!(target: "llama", "{msg}");
    } else {
        tracing::warn!(target: "llama", "{msg}");
    }
}

/// The media marker that [`Embedder::embed`] replaces with one media item.
pub fn marker() -> &'static str {
    unsafe { CStr::from_ptr(sys::mtmd_default_marker()) }
        .to_str()
        .unwrap()
}

/// The query prompt from the model card.
pub fn query_prompt(text: &str) -> String {
    // mtmd would read a typed media marker as a media slot and fail the whole query.
    format!(
        "task: search result | query: {}",
        text.replace(marker(), "")
    )
}

/// The document prompt from the model card.
pub fn document_prompt(title: &str, text: &str) -> String {
    format!("title: {title} | text: {text}")
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("NUL in path {}", path.display()))
}

struct Bitmap(*mut sys::mtmd_bitmap);
impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe { sys::mtmd_bitmap_free(self.0) }
    }
}

struct Chunks(*mut sys::mtmd_input_chunks);
impl Drop for Chunks {
    fn drop(&mut self) {
        unsafe { sys::mtmd_input_chunks_free(self.0) }
    }
}

struct Batch(*mut sys::llama_batch_ext);
impl Batch {
    /// Gives the entry at `idx` its position and marks it for output.
    unsafe fn push(&self, idx: i32, pos: sys::llama_pos) -> Result<()> {
        ensure!(idx >= 0, "llama_batch_ext add failed with {idx}");
        // M-RoPE models read up to 4 positions; this model reads only the first.
        let p = [pos; 4];
        ensure!(
            sys::llama_batch_ext_set_pos(self.0, idx, p.as_ptr()),
            "set_pos failed"
        );
        ensure!(
            sys::llama_batch_ext_set_output_embd(self.0, idx, true),
            "set_output_embd failed"
        );
        Ok(())
    }
}
impl Drop for Batch {
    fn drop(&mut self) {
        unsafe { sys::llama_batch_ext_free(self.0) }
    }
}
