// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use std::cmp;

use engine_traits::{CfNamesExt, CompactExt, ManualCompactionOptions, Result};
use rocksdb::{CompactOptions, CompactionOptions, DBBottommostLevelCompaction, DBCompressionType};

use crate::{engine::RocksEngine, r2e, util};

impl CompactExt for RocksEngine {
    type CompactedEvent = crate::compact_listener::RocksCompactedEvent;

    fn auto_compactions_is_disabled(&self) -> Result<bool> {
        for cf_name in self.cf_names() {
            let cf = util::get_cf_handle(self.as_inner(), cf_name)?;
            if self
                .as_inner()
                .get_options_cf(cf)
                .get_disable_auto_compactions()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn compact_range_cf(
        &self,
        cf: &str,
        start_key: Option<&[u8]>,
        end_key: Option<&[u8]>,
        option: ManualCompactionOptions,
    ) -> Result<()> {
        let db = self.as_inner();
        let handle = util::get_cf_handle(db, cf)?;
        let mut compact_opts = CompactOptions::new();
        // `exclusive_manual == false` means manual compaction can
        // concurrently run with other background compactions.
        compact_opts.set_exclusive_manual_compaction(option.exclusive_manual);
        compact_opts.set_max_subcompactions(option.max_subcompactions as i32);
        if option.bottommost_level_force {
            compact_opts.set_bottommost_level_compaction(DBBottommostLevelCompaction::Force);
        }
        db.compact_range_cf_opt(handle, &compact_opts, start_key, end_key);
        Ok(())
    }

    fn compact_files_in_range_cf(
        &self,
        cf: &str,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
        output_level: Option<i32>,
    ) -> Result<()> {
        let db = self.as_inner();
        let handle = util::get_cf_handle(db, cf)?;
        let cf_opts = db.get_options_cf(handle);
        let output_level = output_level.unwrap_or(cf_opts.get_num_levels() as i32 - 1);

        let mut input_files = Vec::new();
        let cf_meta = db.get_column_family_meta_data(handle);
        for (i, level) in cf_meta.get_levels().iter().enumerate() {
            if i as i32 >= output_level {
                break;
            }
            for f in level.get_files() {
                if end.is_some() && end.unwrap() <= f.get_smallestkey() {
                    continue;
                }
                if start.is_some() && start.unwrap() > f.get_largestkey() {
                    continue;
                }
                input_files.push(f.get_name());
            }
        }
        if input_files.is_empty() {
            return Ok(());
        }

        self.compact_files_cf(
            cf,
            input_files,
            Some(output_level),
            cmp::min(num_cpus::get(), 32) as u32,
            false,
        )
    }

    fn compact_files_cf(
        &self,
        cf: &str,
        mut files: Vec<String>,
        output_level: Option<i32>,
        max_subcompactions: u32,
        exclude_l0: bool,
    ) -> Result<()> {
        let db = self.as_inner();
        let handle = util::get_cf_handle(db, cf)?;
        let cf_opts = db.get_options_cf(handle);
        let output_level = output_level.unwrap_or(cf_opts.get_num_levels() as i32 - 1);
        let output_compression = cf_opts
            .get_compression_per_level()
            .get(output_level as usize)
            .cloned()
            .unwrap_or(DBCompressionType::No);
        let output_file_size_limit = cf_opts.get_target_file_size_base() as usize;

        if exclude_l0 {
            let cf_meta = db.get_column_family_meta_data(handle);
            let l0_files = cf_meta.get_levels()[0].get_files();
            files.retain(|f| !l0_files.iter().any(|n| f.ends_with(&n.get_name())));
        }

        if files.is_empty() {
            return Ok(());
        }

        let mut opts = CompactionOptions::new();
        opts.set_compression(output_compression);
        opts.set_max_subcompactions(max_subcompactions as i32);
        opts.set_output_file_size_limit(output_file_size_limit);

        db.compact_files_cf(handle, &opts, &files, output_level)
            .map_err(r2e)
    }

    fn check_in_range(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<()> {
        self.as_inner().check_in_range(start, end).map_err(r2e)
    }
}

#[cfg(test)]
mod tests {
    use engine_traits::{CfNamesExt, CfOptionsExt, CompactExt, MiscExt, SyncMutable};
    use tempfile::Builder;

    use crate::{RocksCfOptions, RocksDbOptions, util};

    #[test]
    fn test_compact_files_in_range() {
        let temp_dir = Builder::new()
            .prefix("test_compact_files_in_range")
            .tempdir()
            .unwrap();

        let mut cf_opts = RocksCfOptions::default();
        cf_opts.set_disable_auto_compactions(true);
        let cfs_opts = vec![("default", cf_opts.clone()), ("test", cf_opts)];
        let db = util::new_engine_opt(
            temp_dir.path().to_str().unwrap(),
            RocksDbOptions::default(),
            cfs_opts,
        )
        .unwrap();

        for cf_name in db.cf_names() {
            for i in 0..5 {
                db.put_cf(cf_name, &[i], &[i]).unwrap();
                db.put_cf(cf_name, &[i + 1], &[i + 1]).unwrap();
                db.flush_cf(cf_name, true).unwrap();
            }
            let cf = util::get_cf_handle(db.as_inner(), cf_name).unwrap();
            let cf_meta = db.as_inner().get_column_family_meta_data(cf);
            let cf_levels = cf_meta.get_levels();
            assert_eq!(cf_levels.first().unwrap().get_files().len(), 5);
        }

        // # Before
        // Level-0: [4-5], [3-4], [2-3], [1-2], [0-1]
        // # After
        // Level-0: [4-5]
        // Level-1: [0-4]
        db.compact_files_in_range(None, Some(&[4]), Some(1))
            .unwrap();

        for cf_name in db.cf_names() {
            let cf = util::get_cf_handle(db.as_inner(), cf_name).unwrap();
            let cf_meta = db.as_inner().get_column_family_meta_data(cf);
            let cf_levels = cf_meta.get_levels();
            let level_0 = cf_levels[0].get_files();
            assert_eq!(level_0.len(), 1);
            assert_eq!(level_0[0].get_smallestkey(), &[4]);
            assert_eq!(level_0[0].get_largestkey(), &[5]);
            let level_1 = cf_levels[1].get_files();
            assert_eq!(level_1.len(), 1);
            assert_eq!(level_1[0].get_smallestkey(), &[0]);
            assert_eq!(level_1[0].get_largestkey(), &[4]);
        }

        // # Before
        // Level-0: [4-5]
        // Level-1: [0-4]
        // # After
        // Level-0: [4-5]
        // Level-N: [0-4]
        db.compact_files_in_range(Some(&[2]), Some(&[4]), None)
            .unwrap();

        for cf_name in db.cf_names() {
            let cf = util::get_cf_handle(db.as_inner(), cf_name).unwrap();
            let cf_opts = db.get_options_cf(cf_name).unwrap();
            let cf_meta = db.as_inner().get_column_family_meta_data(cf);
            let cf_levels = cf_meta.get_levels();
            let level_0 = cf_levels[0].get_files();
            assert_eq!(level_0.len(), 1);
            assert_eq!(level_0[0].get_smallestkey(), &[4]);
            assert_eq!(level_0[0].get_largestkey(), &[5]);
            let level_n = cf_levels[cf_opts.get_num_levels() - 1].get_files();
            assert_eq!(level_n.len(), 1);
            assert_eq!(level_n[0].get_smallestkey(), &[0]);
            assert_eq!(level_n[0].get_largestkey(), &[4]);
        }
    }
}
