// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Region manifest impl
use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common_telemetry::{info, warn};
use object_store::ObjectStore;
use store_api::manifest::action::ProtocolAction;
use store_api::manifest::{
    Manifest, ManifestLogStorage, ManifestVersion, MetaActionIterator, MIN_VERSION,
};

use crate::error::{ManifestCheckpointSnafu, Result};
use crate::manifest::action::*;
use crate::manifest::checkpoint::Checkpointer;
use crate::manifest::ManifestImpl;

pub type RegionManifest = ManifestImpl<RegionCheckpoint, RegionMetaActionList>;

#[derive(Debug)]
pub struct RegionManifestCheckpointer {
    // The latest manifest version when flushing memtables.
    // Checkpoint can't exceed over flushed manifest version because we have to keep
    // the region metadata for replaying WAL to ensure correct data schema.
    flushed_manifest_version: AtomicU64,
}

impl RegionManifestCheckpointer {
    pub(crate) fn set_flushed_manifest_version(&self, manifest_version: ManifestVersion) {
        self.flushed_manifest_version
            .store(manifest_version, Ordering::Relaxed);
    }
}

#[async_trait]
impl Checkpointer for RegionManifestCheckpointer {
    type Checkpoint = RegionCheckpoint;
    type MetaAction = RegionMetaActionList;

    async fn do_checkpoint(
        &self,
        manifest: &ManifestImpl<RegionCheckpoint, RegionMetaActionList>,
    ) -> Result<Option<RegionCheckpoint>> {
        let last_checkpoint = manifest.last_checkpoint().await?;

        let current_version = manifest.last_version();
        let (start_version, mut protocol, mut manifest_builder) =
            if let Some(checkpoint) = last_checkpoint {
                (
                    checkpoint.last_version + 1,
                    checkpoint.protocol,
                    RegionManifestDataBuilder::with_checkpoint(checkpoint.checkpoint),
                )
            } else {
                (
                    MIN_VERSION,
                    ProtocolAction::default(),
                    RegionManifestDataBuilder::default(),
                )
            };

        let end_version =
            current_version.min(self.flushed_manifest_version.load(Ordering::Relaxed)) + 1;
        if start_version >= end_version {
            return Ok(None);
        }

        let mut iter = manifest.scan(start_version, end_version).await?;

        let mut last_version = start_version;
        let mut compacted_actions = 0;
        while let Some((version, action_list)) = iter.next_action().await? {
            for action in action_list.actions {
                match action {
                    RegionMetaAction::Change(c) => manifest_builder.apply_change(c),
                    RegionMetaAction::Edit(e) => manifest_builder.apply_edit(version, e),
                    RegionMetaAction::Protocol(p) => protocol = p,
                    action => {
                        return ManifestCheckpointSnafu {
                            msg: format!("can't apply region action: {:?}", action),
                        }
                        .fail();
                    }
                }
            }
            last_version = version;
            compacted_actions += 1;
        }

        if compacted_actions == 0 {
            return Ok(None);
        }

        let region_manifest = manifest_builder.build();
        let checkpoint = RegionCheckpoint {
            protocol,
            last_version,
            compacted_actions,
            checkpoint: Some(region_manifest),
        };

        manifest.save_checkpoint(&checkpoint).await?;
        if let Err(e) = manifest
            .manifest_store()
            .delete(start_version, last_version + 1)
            .await
        {
            // We only log when the error kind isn't `NotFound`
            if !e.is_object_to_delete_not_found() {
                // It doesn't matter when deletion fails, they will be purged by gc task.
                warn!(
                    "Failed to delete manifest logs [{},{}] in path: {}. err: {}",
                    start_version,
                    last_version,
                    manifest.manifest_store().path(),
                    e
                );
            }
        }

        info!("Region manifest checkpoint, start_version: {}, last_version: {}, compacted actions: {}", start_version, last_version, compacted_actions);

        Ok(Some(checkpoint))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl RegionManifest {
    pub fn with_checkpointer(
        manifest_dir: &str,
        object_store: ObjectStore,
        checkpoint_actions_margin: Option<u16>,
        gc_duration: Option<Duration>,
    ) -> Self {
        Self::new(
            manifest_dir,
            object_store,
            checkpoint_actions_margin,
            gc_duration,
            Some(Arc::new(RegionManifestCheckpointer {
                flushed_manifest_version: AtomicU64::new(0),
            })),
        )
    }

    // Update flushed manifest version in checkpointer
    pub fn set_flushed_manifest_version(&self, manifest_version: ManifestVersion) {
        if let Some(checkpointer) = self.checkpointer() {
            if let Some(checkpointer) = checkpointer
                .as_any()
                .downcast_ref::<RegionManifestCheckpointer>()
            {
                checkpointer.set_flushed_manifest_version(manifest_version);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use common_test_util::temp_dir::create_temp_dir;
    use object_store::services::Fs;
    use object_store::ObjectStore;
    use store_api::manifest::action::ProtocolAction;
    use store_api::manifest::{Manifest, MetaActionIterator, MAX_VERSION};

    use super::*;
    use crate::manifest::test_utils::*;
    use crate::metadata::RegionMetadata;
    use crate::sst::FileId;

    #[tokio::test]
    async fn test_region_manifest() {
        common_telemetry::init_default_ut_logging();
        let tmp_dir = create_temp_dir("test_region_manifest");
        let mut builder = Fs::default();
        builder.root(&tmp_dir.path().to_string_lossy());
        let object_store = ObjectStore::new(builder).unwrap().finish();

        let manifest = RegionManifest::with_checkpointer("/manifest/", object_store, None, None);
        manifest.start().await.unwrap();

        let region_meta = Arc::new(build_region_meta());

        assert_eq!(
            None,
            manifest
                .scan(0, MAX_VERSION)
                .await
                .unwrap()
                .next_action()
                .await
                .unwrap()
        );

        manifest
            .update(RegionMetaActionList::with_action(RegionMetaAction::Change(
                RegionChange {
                    metadata: region_meta.as_ref().into(),
                    committed_sequence: 99,
                },
            )))
            .await
            .unwrap();

        let mut iter = manifest.scan(0, MAX_VERSION).await.unwrap();

        let (v, action_list) = iter.next_action().await.unwrap().unwrap();
        assert_eq!(0, v);
        assert_eq!(2, action_list.actions.len());
        let protocol = &action_list.actions[0];
        assert!(matches!(
            protocol,
            RegionMetaAction::Protocol(ProtocolAction { .. })
        ));

        let action = &action_list.actions[1];

        match action {
            RegionMetaAction::Change(c) => {
                assert_eq!(
                    RegionMetadata::try_from(c.metadata.clone()).unwrap(),
                    *region_meta
                );
                assert_eq!(c.committed_sequence, 99);
            }
            _ => unreachable!(),
        }

        // Save some actions
        manifest
            .update(RegionMetaActionList::new(vec![
                RegionMetaAction::Edit(build_region_edit(1, &[FileId::random()], &[])),
                RegionMetaAction::Edit(build_region_edit(
                    2,
                    &[FileId::random(), FileId::random()],
                    &[],
                )),
            ]))
            .await
            .unwrap();

        let mut iter = manifest.scan(0, MAX_VERSION).await.unwrap();
        let (v, action_list) = iter.next_action().await.unwrap().unwrap();
        assert_eq!(0, v);
        assert_eq!(2, action_list.actions.len());
        let protocol = &action_list.actions[0];
        assert!(matches!(
            protocol,
            RegionMetaAction::Protocol(ProtocolAction { .. })
        ));

        let action = &action_list.actions[1];
        match action {
            RegionMetaAction::Change(c) => {
                assert_eq!(
                    RegionMetadata::try_from(c.metadata.clone()).unwrap(),
                    *region_meta
                );
                assert_eq!(c.committed_sequence, 99);
            }
            _ => unreachable!(),
        }

        let (v, action_list) = iter.next_action().await.unwrap().unwrap();
        assert_eq!(1, v);
        assert_eq!(2, action_list.actions.len());
        assert!(matches!(&action_list.actions[0], RegionMetaAction::Edit(_)));
        assert!(matches!(&action_list.actions[1], RegionMetaAction::Edit(_)));

        // Reach end
        assert!(iter.next_action().await.unwrap().is_none());

        manifest.stop().await.unwrap();
    }

    async fn assert_scan(manifest: &RegionManifest, start_version: ManifestVersion, expected: u64) {
        let mut iter = manifest.scan(0, MAX_VERSION).await.unwrap();
        let mut actions = 0;
        while let Some((v, _)) = iter.next_action().await.unwrap() {
            assert_eq!(v, start_version + actions);
            actions += 1;
        }
        assert_eq!(expected, actions);
    }

    #[tokio::test]
    async fn test_region_manifest_checkpoint() {
        common_telemetry::init_default_ut_logging();
        let tmp_dir = create_temp_dir("test_region_manifest_checkpoint");
        let mut builder = Fs::default();
        builder.root(&tmp_dir.path().to_string_lossy());
        let object_store = ObjectStore::new(builder).unwrap().finish();

        let manifest = RegionManifest::with_checkpointer(
            "/manifest/",
            object_store,
            None,
            Some(Duration::from_millis(50)),
        );
        manifest.start().await.unwrap();

        let region_meta = Arc::new(build_region_meta());
        let new_region_meta = Arc::new(build_altered_region_meta());

        let file = FileId::random();
        let file_ids = vec![FileId::random(), FileId::random()];

        let actions: Vec<RegionMetaActionList> = vec![
            RegionMetaActionList::with_action(RegionMetaAction::Change(RegionChange {
                metadata: region_meta.as_ref().into(),
                committed_sequence: 1,
            })),
            RegionMetaActionList::new(vec![
                RegionMetaAction::Edit(build_region_edit(2, &[file], &[])),
                RegionMetaAction::Edit(build_region_edit(3, &file_ids, &[file])),
            ]),
            RegionMetaActionList::with_action(RegionMetaAction::Change(RegionChange {
                metadata: new_region_meta.as_ref().into(),
                committed_sequence: 99,
            })),
        ];

        for action in actions {
            manifest.update(action).await.unwrap();
        }
        assert!(manifest.last_checkpoint().await.unwrap().is_none());
        assert_scan(&manifest, 0, 3).await;
        // update flushed manifest version for doing checkpoint
        manifest.set_flushed_manifest_version(2);

        let mut checkpoint_versions = vec![];

        // do a checkpoint
        let checkpoint = manifest.do_checkpoint().await.unwrap().unwrap();
        let last_checkpoint = manifest.last_checkpoint().await.unwrap().unwrap();
        assert_eq!(checkpoint, last_checkpoint);
        assert_eq!(checkpoint.compacted_actions, 3);
        assert_eq!(checkpoint.last_version, 2);
        checkpoint_versions.push(2);
        let alterd_raw_meta = RawRegionMetadata::from(new_region_meta.as_ref());
        assert!(matches!(&checkpoint.checkpoint, Some(RegionManifestData {
            committed_sequence: 99,
            metadata,
            version: Some(RegionVersion {
                manifest_version: 1,
                flushed_sequence: Some(3),
                files,
            }),
        }) if files.len() == 2 &&
                         files.contains_key(&file_ids[0]) &&
                         files.contains_key(&file_ids[1]) &&
                         *metadata == alterd_raw_meta));
        // all actions were compacted
        assert_eq!(
            None,
            manifest
                .scan(0, MAX_VERSION)
                .await
                .unwrap()
                .next_action()
                .await
                .unwrap()
        );

        assert!(manifest.do_checkpoint().await.unwrap().is_none());
        let last_checkpoint = manifest.last_checkpoint().await.unwrap().unwrap();
        assert_eq!(checkpoint, last_checkpoint);

        // add new actions
        let new_file = FileId::random();
        let actions: Vec<RegionMetaActionList> = vec![
            RegionMetaActionList::with_action(RegionMetaAction::Change(RegionChange {
                metadata: region_meta.as_ref().into(),
                committed_sequence: 200,
            })),
            RegionMetaActionList::new(vec![RegionMetaAction::Edit(build_region_edit(
                201,
                &[new_file],
                &file_ids,
            ))]),
        ];
        for action in actions {
            manifest.update(action).await.unwrap();
        }

        assert_scan(&manifest, 3, 2).await;

        // do another checkpoints
        // compacted RegionChange
        manifest.set_flushed_manifest_version(3);
        let checkpoint = manifest.do_checkpoint().await.unwrap().unwrap();
        let last_checkpoint = manifest.last_checkpoint().await.unwrap().unwrap();
        assert_eq!(checkpoint, last_checkpoint);
        assert_eq!(checkpoint.compacted_actions, 1);
        assert_eq!(checkpoint.last_version, 3);
        checkpoint_versions.push(3);
        assert!(matches!(&checkpoint.checkpoint, Some(RegionManifestData {
            committed_sequence: 200,
            metadata,
            version: Some(RegionVersion {
                manifest_version: 1,
                flushed_sequence: Some(3),
                files,
            }),
        }) if files.len() == 2 &&
                         files.contains_key(&file_ids[0]) &&
                         files.contains_key(&file_ids[1]) &&
                         *metadata == RawRegionMetadata::from(region_meta.as_ref())));

        assert_scan(&manifest, 4, 1).await;
        // compacted RegionEdit
        manifest.set_flushed_manifest_version(4);
        let checkpoint = manifest.do_checkpoint().await.unwrap().unwrap();
        let last_checkpoint = manifest.last_checkpoint().await.unwrap().unwrap();
        assert_eq!(checkpoint, last_checkpoint);
        assert_eq!(checkpoint.compacted_actions, 1);
        assert_eq!(checkpoint.last_version, 4);
        checkpoint_versions.push(4);
        assert!(matches!(&checkpoint.checkpoint, Some(RegionManifestData {
            committed_sequence: 200,
            metadata,
            version: Some(RegionVersion {
                manifest_version: 4,
                flushed_sequence: Some(201),
                files,
            }),
        }) if files.len() == 1 &&
                         files.contains_key(&new_file) &&
                         *metadata == RawRegionMetadata::from(region_meta.as_ref())));

        // all actions were compacted
        assert_eq!(
            None,
            manifest
                .scan(0, MAX_VERSION)
                .await
                .unwrap()
                .next_action()
                .await
                .unwrap()
        );

        // wait for gc
        tokio::time::sleep(Duration::from_millis(60)).await;

        for v in checkpoint_versions {
            if v < 4 {
                // ensure old checkpoints were purged.
                assert!(manifest
                    .manifest_store()
                    .load_checkpoint(v)
                    .await
                    .unwrap()
                    .is_none());
            } else {
                // the last checkpoints is still exists.
                let last_checkpoint = manifest.last_checkpoint().await.unwrap().unwrap();
                assert_eq!(checkpoint, last_checkpoint);
            }
        }

        manifest.stop().await.unwrap();
    }
}
