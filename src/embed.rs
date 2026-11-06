//! Static text embeddings (Model2Vec), pure Rust, no dependencies beyond std and serde_json.
//!
//! A Model2Vec model is a WordPiece vocabulary plus one vector per token: a text's embedding is
//! the mean of its tokens' vectors, L2-normalised. mt reads the model's `tokenizer.json` and
//! `model.safetensors` (8-byte header length, JSON header, raw little-endian f32) directly.
//! Default model: minishlab/potion-base-8M (29,528 tokens × 256 dims, 30 MB) in
//! `~/.cache/mt/models/potion-base-8M`, or MT_EMBED_MODEL.
//!
//! On top of the text vectors, `propagate` mixes each node's vector with its neighbours' along
//! the code graph's typed edges (calls, routes, tables, tests), so a vector carries structure:
//! a store function takes on words of the handler and the screen that reach it.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Model { vocab: HashMap<String, u32>, emb: Vec<f32>, pub dim: usize, unk: Option<u32>, prefix: String }

/// MT_EMBED_MODEL, else bge-small (BERT) when mt is built with `bert` and the model is on disk,
/// else potion-base-8M (static).
pub fn model_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("MT_EMBED_MODEL") { return PathBuf::from(p); }
    let base = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/mt/models");
    if cfg!(feature = "bert") && base.join("bge-small-en-v1.5/model.safetensors").exists() { return base.join("bge-small-en-v1.5"); }
    base.join("potion-base-8M")
}

impl Model {
    /// Only the WordPiece tokenizer of a model directory (for BERT, whose weights candle loads).
    pub fn tokenizer_only(dir: &Path) -> Result<Model, String> {
        let tok: serde_json::Value = serde_json::from_str(&fs::read_to_string(dir.join("tokenizer.json")).map_err(|e| format!("tokenizer.json: {e}"))?).map_err(|e| e.to_string())?;
        let m = &tok["model"];
        let vocab: HashMap<String, u32> = m["vocab"].as_object().ok_or("no vocab")?.iter().filter_map(|(k, v)| v.as_u64().map(|i| (k.clone(), i as u32))).collect();
        let unk = m["unk_token"].as_str().and_then(|u| vocab.get(u).copied());
        let prefix = m["continuing_subword_prefix"].as_str().unwrap_or("##").to_string();
        Ok(Model { vocab, emb: vec![], dim: 0, unk, prefix })
    }

    pub fn load(dir: &Path) -> Result<Model, String> {
        let tok: serde_json::Value = serde_json::from_str(&fs::read_to_string(dir.join("tokenizer.json")).map_err(|e| format!("tokenizer.json: {e}"))?).map_err(|e| e.to_string())?;
        let m = &tok["model"];
        if m["type"].as_str() != Some("WordPiece") { return Err(format!("unsupported tokenizer {:?}", m["type"])); }
        let vocab: HashMap<String, u32> = m["vocab"].as_object().ok_or("no vocab")?.iter().filter_map(|(k, v)| v.as_u64().map(|i| (k.clone(), i as u32))).collect();
        let unk = m["unk_token"].as_str().and_then(|u| vocab.get(u).copied());
        let prefix = m["continuing_subword_prefix"].as_str().unwrap_or("##").to_string();
        let raw = fs::read(dir.join("model.safetensors")).map_err(|e| format!("model.safetensors: {e}"))?;
        if raw.len() < 8 { return Err("model.safetensors too short".into()); }
        let hlen = u64::from_le_bytes(raw[..8].try_into().unwrap()) as usize;
        let header: serde_json::Value = serde_json::from_slice(&raw[8..8 + hlen]).map_err(|e| e.to_string())?;
        let t = header.as_object().ok_or("bad header")?.iter().find(|(k, _)| *k != "__metadata__").map(|x| x.1.clone()).ok_or("no tensor")?;
        if t["dtype"].as_str() != Some("F32") { return Err(format!("unsupported dtype {:?}", t["dtype"])); }
        let shape: Vec<usize> = t["shape"].as_array().ok_or("no shape")?.iter().filter_map(|x| x.as_u64().map(|v| v as usize)).collect();
        let off: Vec<usize> = t["data_offsets"].as_array().ok_or("no offsets")?.iter().filter_map(|x| x.as_u64().map(|v| v as usize)).collect();
        let data = &raw[8 + hlen + off[0]..8 + hlen + off[1]];
        let emb: Vec<f32> = data.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
        if shape.len() != 2 || emb.len() != shape[0] * shape[1] { return Err("tensor shape mismatch".into()); }
        Ok(Model { vocab, emb, dim: shape[1], unk, prefix })
    }

    /// BERT-style normalisation and pre-tokenisation (lowercase, split on whitespace and around
    /// every punctuation character), then greedy longest-match WordPiece.
    fn ids(&self, text: &str) -> Vec<u32> {
        let mut out = vec![];
        let lower = text.to_lowercase();
        let mut words: Vec<String> = vec![];
        let mut cur = String::new();
        for ch in lower.chars() {
            if ch.is_whitespace() || ch.is_control() { if !cur.is_empty() { words.push(std::mem::take(&mut cur)); } }
            else if ch.is_ascii_punctuation() || (!ch.is_alphanumeric() && !ch.is_whitespace()) { if !cur.is_empty() { words.push(std::mem::take(&mut cur)); } words.push(ch.to_string()); }
            else { cur.push(ch); }
        }
        if !cur.is_empty() { words.push(cur); }
        for w in words {
            let chars: Vec<char> = w.chars().collect();
            if chars.len() > 100 { if let Some(u) = self.unk { out.push(u); } continue; }
            let (mut start, mut pieces, mut bad) = (0, vec![], false);
            while start < chars.len() {
                let mut end = chars.len();
                let mut found = None;
                while start < end {
                    let mut s: String = chars[start..end].iter().collect();
                    if start > 0 { s = format!("{}{s}", self.prefix); }
                    if let Some(&i) = self.vocab.get(&s) { found = Some(i); break; }
                    end -= 1;
                }
                match found { Some(i) => { pieces.push(i); start = end; } None => { bad = true; break; } }
            }
            if bad { if let Some(u) = self.unk { out.push(u); } } else { out.extend(pieces); }
        }
        out
    }

    /// Mean of the token vectors, L2-normalised; unknown tokens are skipped. Zero vector for text
    /// with no known token.
    pub fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        let mut n = 0usize;
        for id in self.ids(text) {
            if Some(id) == self.unk { continue; }
            let row = &self.emb[id as usize * self.dim..(id as usize + 1) * self.dim];
            for (a, b) in v.iter_mut().zip(row) { *a += b; }
            n += 1;
        }
        if n > 0 { normalize(&mut v); }
        v
    }
}

// ---------------------------------------------------------------- BERT (candle, pure Rust)

/// A text encoder: the static Model2Vec table, or (with `--features bert`) a BERT model run on the
/// CPU by candle. Which one is decided by the model directory's config.json.
pub enum Embedder { Static(Model), #[cfg(feature = "bert")] Bert(Bert) }

impl Embedder {
    pub fn load(dir: &Path) -> Result<Embedder, String> {
        let cfg: serde_json::Value = fs::read_to_string(dir.join("config.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(serde_json::Value::Null);
        if cfg["model_type"].as_str() == Some("bert") {
            #[cfg(feature = "bert")] { return Bert::load(dir).map(Embedder::Bert); }
            #[cfg(not(feature = "bert"))] { return Err("a BERT model needs mt built with --features bert".into()); }
        }
        Model::load(dir).map(Embedder::Static)
    }
    pub fn dim(&self) -> usize { match self { Embedder::Static(m) => m.dim, #[cfg(feature = "bert")] Embedder::Bert(b) => b.dim } }
    pub fn name(&self) -> &'static str { match self { Embedder::Static(_) => "model2vec", #[cfg(feature = "bert")] Embedder::Bert(_) => "bert" } }
    /// Cheap enough to embed every definition on every prompt (static tables only).
    pub fn is_cheap(&self) -> bool { matches!(self, Embedder::Static(_)) }
    pub fn embed_docs(&self, texts: &[String]) -> Vec<Vec<f32>> {
        match self { Embedder::Static(m) => texts.iter().map(|t| m.embed(t)).collect(), #[cfg(feature = "bert")] Embedder::Bert(b) => b.embed_batch(texts) }
    }
    pub fn embed_query(&self, q: &str) -> Vec<f32> {
        match self {
            Embedder::Static(m) => m.embed(q),
            // bge's retrieval instruction for short queries
            #[cfg(feature = "bert")] Embedder::Bert(b) => b.embed_batch(&[format!("Represent this sentence for searching relevant passages: {q}")]).pop().unwrap_or_default(),
        }
    }
}

#[cfg(feature = "bert")]
pub struct Bert { tok: Model, model: candle_transformers::models::bert::BertModel, pub dim: usize, cls: u32, sep: u32 }

#[cfg(feature = "bert")]
impl Bert {
    pub fn load(dir: &Path) -> Result<Bert, String> {
        use candle_core::{DType, Device};
        let cfg_s = fs::read_to_string(dir.join("config.json")).map_err(|e| e.to_string())?;
        let cfg: candle_transformers::models::bert::Config = serde_json::from_str(&cfg_s).map_err(|e| format!("config.json: {e}"))?;
        let dim = serde_json::from_str::<serde_json::Value>(&cfg_s).ok().and_then(|v| v["hidden_size"].as_u64()).unwrap_or(384) as usize;
        let tok = Model::tokenizer_only(dir)?;
        let cls = *tok.vocab.get("[CLS]").ok_or("no [CLS]")?;
        let sep = *tok.vocab.get("[SEP]").ok_or("no [SEP]")?;
        // SAFETY: the weights file is only read, and outlives the model for the process
        let vb = unsafe { candle_nn::VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DType::F32, &Device::Cpu) }.map_err(|e| e.to_string())?;
        let model = candle_transformers::models::bert::BertModel::load(vb, &cfg).map_err(|e| e.to_string())?;
        Ok(Bert { tok, model, dim, cls, sep })
    }

    /// [CLS] text [SEP], at most 64 tokens, batches of 32 padded to the longest; the [CLS] vector,
    /// L2-normalised (how bge is trained to be used).
    pub fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        use candle_core::{Device, Tensor};
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(32) {
            let seqs: Vec<Vec<u32>> = chunk.iter().map(|t| {
                let mut ids = vec![self.cls];
                ids.extend(self.tok.ids(t).into_iter().take(62));
                ids.push(self.sep);
                ids
            }).collect();
            let len = seqs.iter().map(|s| s.len()).max().unwrap_or(2);
            let (mut ids, mut mask) = (Vec::with_capacity(chunk.len() * len), Vec::with_capacity(chunk.len() * len));
            for s in &seqs { for i in 0..len { ids.push(*s.get(i).unwrap_or(&0)); mask.push(if i < s.len() { 1u32 } else { 0 }); } }
            let run = || -> candle_core::Result<Vec<Vec<f32>>> {
                let ids = Tensor::from_vec(ids.clone(), (chunk.len(), len), &Device::Cpu)?;
                let mask = Tensor::from_vec(mask.clone(), (chunk.len(), len), &Device::Cpu)?;
                let tt = ids.zeros_like()?;
                let h = self.model.forward(&ids, &tt, Some(&mask))?;
                h.narrow(1, 0, 1)?.squeeze(1)?.to_vec2::<f32>()
            };
            match run() {
                Ok(vs) => for mut v in vs { normalize(&mut v); out.push(v); },
                Err(e) => { eprintln!("mt embed: {e}"); for _ in chunk { out.push(vec![0.0; self.dim]); } }
            }
        }
        out
    }
}

pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 { for x in v.iter_mut() { *x /= n; } }
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() }

/// Graph propagation without training: `rounds` times, every node becomes
/// `self_w * own + (1 - self_w) * weighted mean of its neighbours`, renormalised. Edges are
/// undirected and weighted by type; nodes without edges keep their own vector.
pub fn propagate(vecs: &[Vec<f32>], edges: &[(usize, usize, f32)], self_w: f32, rounds: usize) -> Vec<Vec<f32>> {
    let n = vecs.len();
    let mut adj: Vec<Vec<(usize, f32)>> = vec![vec![]; n];
    for &(a, b, w) in edges { if a != b && a < n && b < n { adj[a].push((b, w)); adj[b].push((a, w)); } }
    let mut cur: Vec<Vec<f32>> = vecs.to_vec();
    for _ in 0..rounds {
        let next: Vec<Vec<f32>> = (0..n).map(|i| {
            if adj[i].is_empty() { return cur[i].clone(); }
            let wsum: f32 = adj[i].iter().map(|x| x.1).sum();
            let mut v: Vec<f32> = vecs[i].iter().map(|x| x * self_w).collect();
            for &(j, w) in &adj[i] { for (a, b) in v.iter_mut().zip(&cur[j]) { *a += (1.0 - self_w) * w / wsum * b; } }
            normalize(&mut v);
            v
        }).collect();
        cur = next;
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propagation_pulls_a_node_toward_its_neighbours() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let out = propagate(&[a.clone(), b.clone(), vec![0.6, 0.8]], &[(0, 1, 1.0)], 0.5, 1);
        assert!(out[0][1] > 0.4 && out[0][0] > 0.4, "{:?}", out[0]);
        assert_eq!(out[2], vec![0.6, 0.8], "a node with no edges keeps its own vector");
    }

    #[test]
    fn model_embeds_related_words_closer_when_present() {
        let Ok(m) = Model::load(&model_dir()) else { eprintln!("no model at {}: skipped", model_dir().display()); return };
        let q = m.embed("log the user out");
        let near = m.embed("revoke session");
        let far = m.embed("render chart colors");
        assert!(dot(&q, &near) > dot(&q, &far), "{} vs {}", dot(&q, &near), dot(&q, &far));
    }
}
