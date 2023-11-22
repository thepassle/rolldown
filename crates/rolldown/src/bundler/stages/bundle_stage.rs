use std::{borrow::Cow, hash::BuildHasherDefault};

use crate::{
  bundler::{
    bundle::output::OutputChunk,
    chunk::{
      chunk::{Chunk, CrossChunkImportItem},
      ChunkId, ChunksVec,
    },
    chunk_graph::ChunkGraph,
    module::Module,
    options::output_options::OutputOptions,
    stages::link_stage::LinkStageOutput,
    utils::bitset::BitSet,
  },
  InputOptions, Output, OutputFormat,
};
use index_vec::{index_vec, IndexVec};
use rolldown_common::{EntryPointKind, ExportsKind, ImportKind, ModuleId, NamedImport, SymbolRef};
use rustc_hash::{FxHashMap, FxHashSet};

pub struct BundleStage<'a> {
  link_output: &'a mut LinkStageOutput,
  output_options: &'a OutputOptions,
  input_options: &'a InputOptions,
}

impl<'a> BundleStage<'a> {
  pub fn new(
    link_output: &'a mut LinkStageOutput,
    input_options: &'a InputOptions,
    output_options: &'a OutputOptions,
  ) -> Self {
    Self { link_output, output_options, input_options }
  }

  #[tracing::instrument(skip_all)]
  pub fn bundle(&mut self) -> Vec<Output> {
    use rayon::prelude::*;
    let mut chunk_graph = self.generate_chunks();

    chunk_graph
      .chunks
      .iter_mut()
      .par_bridge()
      .for_each(|chunk| chunk.render_file_name(self.output_options));

    self.compute_cross_chunk_links(&mut chunk_graph);

    chunk_graph.chunks.iter_mut().par_bridge().for_each(|chunk| {
      chunk.de_conflict(self.link_output);
    });

    let assets = chunk_graph
      .chunks
      .iter()
      .enumerate()
      .map(|(_chunk_id, c)| {
        let (content, rendered_modules) = c
          .render(self.input_options, self.link_output, &chunk_graph, self.output_options)
          .unwrap();

        Output::Chunk(Box::new(OutputChunk {
          file_name: c.file_name.clone().unwrap(),
          code: content,
          is_entry: matches!(&c.entry_point, Some(e) if e.kind == EntryPointKind::UserSpecified),
          facade_module_id: c.entry_point.as_ref().map(|entry_point| {
            self.link_output.modules[entry_point.module_id].expect_normal().pretty_path.to_string()
          }),
          modules: rendered_modules,
          exports: c.get_export_names(self.link_output, self.output_options),
        }))
      })
      .collect::<Vec<_>>();

    assets
  }

  fn determine_reachable_modules_for_entry(
    &self,
    module_id: ModuleId,
    entry_index: u32,
    module_to_bits: &mut IndexVec<ModuleId, BitSet>,
  ) {
    if module_to_bits[module_id].has_bit(entry_index) {
      return;
    }
    module_to_bits[module_id].set_bit(entry_index);
    let Module::Normal(module) = &self.link_output.modules[module_id] else { return };
    module.import_records.iter().for_each(|rec| {
      // Module imported dynamically will be considered as an entry,
      // so we don't need to include it in this chunk
      if rec.kind != ImportKind::DynamicImport {
        self.determine_reachable_modules_for_entry(
          rec.resolved_module,
          entry_index,
          module_to_bits,
        );
      }
    });
  }

  // TODO(hyf0): refactor this function
  #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
  fn compute_cross_chunk_links(&mut self, chunk_graph: &mut ChunkGraph) {
    // Determine which symbols belong to which chunk
    let mut chunk_meta_imports_vec =
      index_vec![FxHashSet::<SymbolRef>::default(); chunk_graph.chunks.len()];
    let mut chunk_meta_exports_vec =
      index_vec![FxHashSet::<SymbolRef>::default(); chunk_graph.chunks.len()];
    let mut chunk_meta_imports_from_external_modules_vec =
      index_vec![FxHashMap::<ModuleId, Vec<NamedImport>>::default(); chunk_graph.chunks.len()];
    for (chunk_id, chunk) in chunk_graph.chunks.iter_enumerated() {
      let chunk_meta_imports = &mut chunk_meta_imports_vec[chunk_id];
      let imports_from_external_modules =
        &mut chunk_meta_imports_from_external_modules_vec[chunk_id];

      for module_id in chunk.modules.iter().copied() {
        match &self.link_output.modules[module_id] {
          Module::Normal(module) => {
            module.import_records.iter().for_each(|rec| {
              match &self.link_output.modules[rec.resolved_module] {
                Module::External(importee) if matches!(rec.kind, ImportKind::Import) => {
                  // Make sure the side effects of external module are evaluated.
                  imports_from_external_modules.entry(importee.id).or_default();
                }
                _ => {}
              }
            });
            module.named_imports.iter().for_each(|(_, import)| {
              let rec = &module.import_records[import.record_id];
              if let Module::External(importee) = &self.link_output.modules[rec.resolved_module] {
                imports_from_external_modules.entry(importee.id).or_default().push(import.clone());
              }
            });
            for stmt_info in module.stmt_infos.iter() {
              for declared in &stmt_info.declared_symbols {
                let symbol = self.link_output.symbols.get_mut(*declared);
                debug_assert!(
                  symbol.chunk_id.unwrap_or(chunk_id) == chunk_id,
                  "Symbol: {:?}, {:?} in {:?} should only be declared in one chunk",
                  symbol.name,
                  declared,
                  module.resource_id,
                );

                self.link_output.symbols.get_mut(*declared).chunk_id = Some(chunk_id);
              }

              if !stmt_info.is_included {
                continue;
              }

              for referenced in &stmt_info.referenced_symbols {
                let canonical_ref = self.link_output.symbols.canonical_ref_for(*referenced);
                chunk_meta_imports.insert(canonical_ref);
              }
            }
          }
          Module::External(_) => {
            // TODO: process external module
          }
        }
      }

      if let Some(entry_point) = &chunk.entry_point {
        let entry_module = &self.link_output.modules[entry_point.module_id];
        let Module::Normal(entry_module) = entry_module else {
          return;
        };
        let entry_linking_info = &self.link_output.linking_infos[entry_module.id];
        if matches!(entry_module.exports_kind, ExportsKind::CommonJs)
          && matches!(self.output_options.format, OutputFormat::Esm)
        {
          chunk_meta_imports
            .insert(entry_linking_info.wrapper_ref.expect("cjs should be wrapped in esm output"));
        }
        for export_ref in entry_linking_info.resolved_exports.values() {
          let mut canonical_ref = self.link_output.symbols.canonical_ref_for(export_ref.symbol_ref);
          let symbol = self.link_output.symbols.get(canonical_ref);
          if let Some(ns_alias) = &symbol.namespace_alias {
            canonical_ref = ns_alias.namespace_ref;
          }
          chunk_meta_imports.insert(canonical_ref);
        }
      }
    }

    for (chunk_id, chunk) in chunk_graph.chunks.iter_mut_enumerated() {
      let chunk_meta_imports = &chunk_meta_imports_vec[chunk_id];
      for import_ref in chunk_meta_imports.iter().copied() {
        let import_symbol = self.link_output.symbols.get(import_ref);
        let importee_chunk_id =
          import_symbol.chunk_id.expect("Symbol should be declared in a chunk");
        // Find out the import_ref whether comes from the chunk or external module.
        if chunk_id != importee_chunk_id {
          chunk
            .imports_from_other_chunks
            .entry(importee_chunk_id)
            .or_default()
            .push(CrossChunkImportItem { import_ref, export_alias: None });
          chunk_meta_exports_vec[importee_chunk_id].insert(import_ref);
        }
      }

      if chunk.entry_point.is_none() {
        continue;
      }
      // If this is an entry point, make sure we import all chunks belonging to
      // this entry point, even if there are no imports. We need to make sure
      // these chunks are evaluated for their side effects too.
      // TODO: ensure chunks are evaluated for their side effects too.
    }
    // Generate cross-chunk exports. These must be computed before cross-chunk
    // imports because of export alias renaming, which must consider all export
    // aliases simultaneously to avoid collisions.
    let mut name_count = FxHashMap::default();
    for (chunk_id, chunk) in chunk_graph.chunks.iter_mut_enumerated() {
      for export in chunk_meta_exports_vec[chunk_id].iter().copied() {
        let original_name = self.link_output.symbols.get_original_name(export);
        let count = name_count.entry(Cow::Borrowed(original_name)).or_insert(0u32);
        let alias = if *count == 0 {
          original_name.clone()
        } else {
          format!("{original_name}${count}").into()
        };
        chunk.exports_to_other_chunks.insert(export, alias.clone());
        *count += 1;
      }
    }
    for chunk_id in chunk_graph.chunks.indices() {
      for (importee_chunk_id, import_items) in
        &chunk_graph.chunks[chunk_id].imports_from_other_chunks
      {
        for item in import_items {
          if let Some(alias) =
            chunk_graph.chunks[*importee_chunk_id].exports_to_other_chunks.get(&item.import_ref)
          {
            // safety: no other mutable reference to `item` exists
            unsafe {
              let item = (item as *const CrossChunkImportItem).cast_mut();
              (*item).export_alias = Some(alias.clone().into());
            }
          }
        }
      }
    }

    chunk_meta_imports_from_external_modules_vec.into_iter_enumerated().for_each(
      |(chunk_id, imports_from_external_modules)| {
        chunk_graph.chunks[chunk_id].imports_from_external_modules = imports_from_external_modules;
      },
    );
  }

  fn generate_chunks(&self) -> ChunkGraph {
    let entries_len: u32 = self.link_output.entries.len().try_into().unwrap();

    let mut module_to_bits = index_vec::index_vec![
      BitSet::new(entries_len);
      self.link_output.modules.len()
    ];
    let mut bits_to_chunk = FxHashMap::with_capacity_and_hasher(
      self.link_output.entries.len(),
      BuildHasherDefault::default(),
    );
    let mut chunks = ChunksVec::with_capacity(self.link_output.entries.len());

    // Create chunk for each static and dynamic entry
    for (entry_index, entry_point) in self.link_output.entries.iter().enumerate() {
      let count: u32 = u32::try_from(entry_index).unwrap();
      let mut bits = BitSet::new(entries_len);
      bits.set_bit(count);
      let chunk = chunks.push(Chunk::new(
        entry_point.name.clone(),
        Some(entry_point.clone()),
        bits.clone(),
        vec![],
      ));
      bits_to_chunk.insert(bits, chunk);
    }

    // Determine which modules belong to which chunk. A module could belong to multiple chunks.
    self.link_output.entries.iter().enumerate().for_each(|(i, entry_point)| {
      // runtime module are shared by all chunks, so we mark it as reachable for all entries.
      // FIXME: But this solution is not perfect. If we have two entries, one of them relies on runtime module, the other one doesn't.
      // In this case, we only need to generate two chunks, but currently we will generate three chunks. We need to analyze the usage of runtime module
      // to make sure only necessary chunks mark runtime module as reachable.
      self.determine_reachable_modules_for_entry(
        self.link_output.runtime.id(),
        i.try_into().unwrap(),
        &mut module_to_bits,
      );

      self.determine_reachable_modules_for_entry(
        entry_point.module_id,
        i.try_into().unwrap(),
        &mut module_to_bits,
      );
    });

    let mut module_to_chunk: IndexVec<ModuleId, Option<ChunkId>> = index_vec::index_vec![
      None;
      self.link_output.modules.len()
    ];

    // FIXME: should remove this when tree shaking is supported
    let is_rolldown_test = std::env::var("ROLLDOWN_TEST").is_ok();
    if is_rolldown_test {
      let runtime_chunk_id = chunks.push(Chunk::new(
        Some("_rolldown_runtime".to_string()),
        None,
        BitSet::new(0),
        vec![self.link_output.runtime.id()],
      ));
      module_to_chunk[self.link_output.runtime.id()] = Some(runtime_chunk_id);
    }

    // 1. Assign modules to corresponding chunks
    // 2. Create shared chunks to store modules that belong to multiple chunks.
    for module in &self.link_output.modules {
      // FIXME: should remove this when tree shaking is supported
      if is_rolldown_test && module.id() == self.link_output.runtime.id() {
        continue;
      }
      let bits = &module_to_bits[module.id()];
      if let Some(chunk_id) = bits_to_chunk.get(bits).copied() {
        chunks[chunk_id].modules.push(module.id());
        module_to_chunk[module.id()] = Some(chunk_id);
      } else {
        let len = bits_to_chunk.len();
        // FIXME: https://github.com/rolldown-rs/rolldown/issues/49
        let chunk = Chunk::new(Some(len.to_string()), None, bits.clone(), vec![module.id()]);
        let chunk_id = chunks.push(chunk);
        module_to_chunk[module.id()] = Some(chunk_id);
        bits_to_chunk.insert(bits.clone(), chunk_id);
      }
    }

    // Sort modules in each chunk by execution order
    chunks.iter_mut().for_each(|chunk| {
      chunk.modules.sort_by_key(|module_id| self.link_output.modules[*module_id].exec_order());
    });

    ChunkGraph { chunks, module_to_chunk }
  }
}
