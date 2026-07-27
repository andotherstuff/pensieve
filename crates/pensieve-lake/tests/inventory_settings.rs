use pensieve_lake::{Error, Inventory};

#[test]
fn immutable_inventory_setting_survives_reopen_and_rejects_drift() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("inventory.sqlite");

    let mut inventory = Inventory::open(&path).expect("open inventory");
    inventory
        .ensure_setting("parquet_shadow.replay_policy.v1", "from-segment:42")
        .expect("record setting");
    assert_eq!(
        inventory
            .setting("parquet_shadow.replay_policy.v1")
            .expect("load setting")
            .as_deref(),
        Some("from-segment:42")
    );
    assert_eq!(
        inventory.setting("missing").expect("load missing setting"),
        None
    );
    drop(inventory);

    let mut reopened = Inventory::open(path).expect("reopen inventory");
    reopened
        .ensure_setting("parquet_shadow.replay_policy.v1", "from-segment:42")
        .expect("same setting is idempotent");
    let error = reopened
        .ensure_setting("parquet_shadow.replay_policy.v1", "all")
        .expect_err("setting drift must fail");

    assert!(matches!(
        error,
        Error::InventorySettingConflict {
            key,
            requested,
            actual,
        } if key == "parquet_shadow.replay_policy.v1"
            && requested == "all"
            && actual == "from-segment:42"
    ));
}
