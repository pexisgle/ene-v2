use super::*;
use bevy::asset::{AssetLoader, LoadContext, io::Reader};
use std::time::{Duration, Instant};

#[derive(Asset, TypePath)]
struct LocalAsset(Vec<u8>);

#[derive(TypePath)]
struct LocalLoader;

impl AssetLoader for LocalLoader {
    type Asset = LocalAsset;
    type Settings = ();
    type Error = std::io::Error;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &(),
        _context: &mut LoadContext<'_>,
    ) -> Result<LocalAsset, std::io::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        Ok(LocalAsset(bytes))
    }

    fn extensions(&self) -> &[&str] {
        &["vrm", "vrma"]
    }
}

#[test]
fn selected_absolute_files_load_without_losing_prefixes_or_interpreting_labels() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, local_asset_plugin()))
        .init_asset::<LocalAsset>()
        .register_asset_loader(LocalLoader);

    // FileAssetReader must actually read the chosen file, including on Windows,
    // rather than resolve it relative to the executable or parse # as a label.
    for extension in ["vrm", "vrma"] {
        let path = directory.path().join(format!("選択 # sample.{extension}"));
        std::fs::write(&path, extension.as_bytes()).unwrap();
        let selected = local_path(path.to_str().unwrap()).unwrap();
        let handle: Handle<LocalAsset> = app
            .world()
            .resource::<AssetServer>()
            .load_builder()
            .override_unapproved()
            .load(AssetPath::from(selected));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            app.update();
            if let Some(asset) = app.world().resource::<Assets<LocalAsset>>().get(&handle) {
                assert_eq!(asset.0, extension.as_bytes());
                break;
            }
            assert!(Instant::now() < deadline, "Selected file failed to load");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
