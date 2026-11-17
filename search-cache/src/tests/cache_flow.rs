use super::prelude::*;
use cardinal_sdk::{EventFlag, FsEvent};
use std::sync::atomic::AtomicBool;

static NEVER_STOPPED: AtomicBool = AtomicBool::new(false);

#[test]
fn test_search_empty_returns_all_nodes() {
    let tmp = TempDir::new("search_empty").unwrap();
    fs::File::create(tmp.path().join("a.txt")).unwrap();
    fs::File::create(tmp.path().join("b.txt")).unwrap();
    let cache = SearchCache::walk_fs(tmp.path());
    let all = cache
        .search_empty(CancellationToken::noop())
        .expect("noop cancellation token should not cancel");
    assert_eq!(all.len(), cache.get_total_files());
}

#[test]
fn test_node_path_root_and_child() {
    let tmp = TempDir::new("node_path").unwrap();
    fs::create_dir(tmp.path().join("dir1")).unwrap();
    fs::File::create(tmp.path().join("dir1/file_x")).unwrap();
    let mut cache = SearchCache::walk_fs(tmp.path());
    let idxs = cache.search("file_x").unwrap();
    assert_eq!(idxs.len(), 1);
    let full = cache.node_path(idxs.into_iter().next().unwrap()).unwrap();
    assert!(full.ends_with(PathBuf::from("dir1/file_x")));
}

#[test]
fn test_remove_node_path_nonexistent_returns_none() {
    let tmp = TempDir::new("remove_node_none").unwrap();
    let mut cache = SearchCache::walk_fs(tmp.path());
    // remove_node_path is private via crate; exercise via scan removal scenario
    // create then delete file and ensure second scan removal returns None
    let file = tmp.path().join("temp_remove.txt");
    fs::write(&file, b"x").unwrap();
    let id = cache.last_event_id() + 1;
    cache
        .handle_fs_events(vec![FsEvent {
            path: file.clone(),
            id,
            flag: EventFlag::ItemCreated,
        }])
        .unwrap();
    // delete file and send removal event => handle_fs_events will trigger internal removal
    fs::remove_file(&file).unwrap();
    let id2 = id + 1;
    cache
        .handle_fs_events(vec![FsEvent {
            path: file.clone(),
            id: id2,
            flag: EventFlag::ItemRemoved,
        }])
        .unwrap();
    assert!(cache.search("temp_remove.txt").unwrap().is_empty());
}

#[test]
fn test_expand_file_nodes_fetch_metadata() {
    let tmp = TempDir::new("expand_meta").unwrap();
    fs::write(tmp.path().join("meta.txt"), b"hello world").unwrap();
    let mut cache = SearchCache::walk_fs(tmp.path());
    let idxs = cache.search("meta.txt").unwrap();
    assert_eq!(idxs.len(), 1);
    // First query_files returns metadata None
    let q1 = cache
        .query_files("meta.txt", CancellationToken::noop())
        .expect("query should succeed")
        .expect("noop cancellation token should not cancel");
    assert_eq!(q1.len(), 1);
    assert!(q1[0].metadata.is_none());
    // expand_file_nodes should fetch metadata
    let nodes = cache.expand_file_nodes(&idxs);
    assert_eq!(nodes.len(), 1);
    assert!(
        nodes[0].metadata.is_some(),
        "metadata should be fetched on demand"
    );
    // A second expand should still have metadata (cached)
    let nodes2 = cache.expand_file_nodes(&idxs);
    assert!(nodes2[0].metadata.is_some());
}

#[test]
fn test_persistent_roundtrip() {
    let tmp = TempDir::new("persist_round").unwrap();
    fs::write(tmp.path().join("a.bin"), b"data").unwrap();
    let cache_path = tmp.path().join("cache.zstd");
    let cache = SearchCache::walk_fs(tmp.path());
    let original_total = cache.get_total_files();
    cache.flush_to_file(&cache_path).unwrap();
    let loaded = SearchCache::try_read_persistent_cache(
        tmp.path(),
        &cache_path,
        &Vec::new(),
        &Vec::new(),
        &NEVER_STOPPED,
    )
    .unwrap();
    assert_eq!(loaded.get_total_files(), original_total);
}

/// 目录型忽略规则（`.*/`）不能被 FSEvents 增量扫描绕过。
///
/// 现象：冷启动加载持久化缓存后，`.config` 之类点开头的目录又出现在结果里；
/// 到设置里重新保存一次（触发全盘重建）才消失。
/// 根因：`scan_path_recursive` 只判目标 entry 自己的名字，而 `create_node_chain`
/// 逐级补父链时完全不看规则，于是"点目录下某个文件"的事件把被排除的祖先目录
/// 重新挂回索引，随后被写进持久化缓存。
#[test]
fn incremental_event_under_ignored_dir_does_not_revive_it() {
    use cardinal_sdk::{EventFlag, FsEvent};
    use fswalk::{FileNameRules, WalkData};
    use std::{
        ffi::OsStr,
        path::Path,
        sync::{Arc, atomic::Ordering},
    };

    fn cache_with_dir_rules(root: &Path) -> SearchCache {
        let rules = Arc::new(FileNameRules::from_lines([".*/"]));
        let empty: Vec<PathBuf> = Vec::new();
        let walk_data = WalkData::new(root, &empty, &empty, false, || {
            NEVER_STOPPED.load(Ordering::Relaxed)
        })
        .with_file_name_rules(rules);
        SearchCache::walk_fs_with_walk_data(&walk_data, &NEVER_STOPPED).unwrap()
    }

    fn contains_segment(cache: &SearchCache, name: &str) -> bool {
        cache
            .search_empty(CancellationToken::noop())
            .unwrap()
            .iter()
            .any(|&idx| {
                cache.node_path(idx).is_some_and(|p| {
                    p.components()
                        .any(|c| c.as_os_str() == OsStr::new(name))
                })
            })
    }

    let tmp = TempDir::new("ignore_dir_incremental").unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".config/inner")).unwrap();
    fs::write(root.join(".config/inner/note.txt"), b"needle").unwrap();
    fs::create_dir_all(root.join("normal")).unwrap();
    fs::write(root.join("normal/ok.txt"), b"needle").unwrap();

    let mut cache = cache_with_dir_rules(root);
    assert!(
        !contains_segment(&cache, ".config"),
        "初次全量扫描就应排除 .config"
    );
    assert!(contains_segment(&cache, "normal"));

    // FSEvents 通常只报告被排除目录下某个具体文件，而不是那个目录本身。
    let id = cache.last_event_id() + 1;
    cache
        .handle_fs_events(vec![FsEvent {
            path: root.join(".config/inner/note.txt"),
            id,
            flag: EventFlag::ItemCreated,
        }])
        .unwrap();

    assert!(
        !contains_segment(&cache, ".config"),
        "增量事件不得把被忽略的点目录（及其父链）重新挂回索引"
    );
    assert!(
        cache.search("note.txt").unwrap().is_empty(),
        "被排除目录下的文件也不应重新进入索引"
    );
}
