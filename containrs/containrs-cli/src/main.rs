use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{App, AppSettings, Arg, SubCommand};
use failure::ResultExt;
use futures::future;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use lazy_static::lazy_static;
use oci_image::v1 as ociv1;
use tokio::fs::{self, File};
use tokio::prelude::*;

use docker_reference::{Reference, ReferenceKind};

use containrs::{Blob, Client, Credentials, Paginate};

mod parse_range;
use crate::parse_range::ParsableRange;

#[tokio::main]
async fn main() {
    if let Err(fail) = true_main().await {
        println!("{}", fail);
        for cause in fail.iter_causes() {
            println!("\tcaused by: {}", cause);
        }
    }
}

lazy_static! {
    static ref PB_STYLE: ProgressStyle = ProgressStyle::default_bar()
        .template("[{elapsed_precise}] {msg:16} - {total_bytes:8} {wide_bar} [{percent:3}%]",)
        .progress_chars("##-");
}

async fn true_main() -> Result<(), failure::Error> {
    pretty_env_logger::init();

    let app_m = App::new("containrs-cli")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .version("0.1.0")
        .author("Azure IoT Edge Devs")
        .about("CLI for interacting with the containrs library.")
        .arg(
            Arg::with_name("transport-scheme")
                .help("Transport scheme (defaults to \"https\")")
                .long("transport-scheme")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("default-registry")
                .help("Default registry (defaults to \"registry-1.docker.io\")")
                .long("default-registry")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("username")
                .help("Username (for use with UserPass Credentials)")
                .short("u")
                .long("username")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("password")
                .help("Password (for use with UserPass Credentials)")
                .short("p")
                .long("password")
                .takes_value(true),
        )
        .subcommand(
            SubCommand::with_name("raw")
                .setting(AppSettings::SubcommandRequiredElseHelp)
                .about("Retrieve raw responses from various endpoints")
                .subcommand(
                    SubCommand::with_name("catalog")
                        .about("Retrieve a sorted list of repositories available in the registry")
                        .arg(
                            Arg::with_name("n")
                                .help("Paginate results into n-sized chunks")
                                .index(1),
                        ),
                )
                .subcommand(
                    SubCommand::with_name("tags")
                        .about("Retrieve a sorted list of tags under a given repository")
                        .arg(
                            Arg::with_name("repo")
                                .help("Repository")
                                .required(true)
                                .index(1),
                        )
                        .arg(
                            Arg::with_name("n")
                                .help("Paginate results into n-sized chunks")
                                .index(2),
                        ),
                )
                .subcommand(
                    SubCommand::with_name("manifest")
                        .about("Retrieve an image's manifest")
                        .arg(
                            Arg::with_name("image")
                                .help("Image reference")
                                .required(true)
                                .index(1),
                        ),
                )
                .subcommand(
                    SubCommand::with_name("blob")
                        .about("Retrieve a blob from a given repository")
                        .arg(
                            Arg::with_name("repo@digest")
                                .help("A string of form repo@digest (e.g: ubuntu@sha256:...)")
                                .required(true)
                                .index(1),
                        )
                        .arg(
                            Arg::with_name("range")
                                .help("A range of bytes to retrieve (e.g: \"10..\" will return everything except the first 9 bytes)")
                                .index(2),
                        ),
                ),
        )
        .subcommand(
            SubCommand::with_name("download")
                .about("Downloads an image onto disk")

                .arg(
                    Arg::with_name("image")
                        .help("Image reference")
                        .required(true)
                        .index(1),
                )
                .arg(
                    Arg::with_name("outdir")
                        .help("Output directory")
                        .required(true)
                        .index(2),
                )
        )
        .get_matches();

    // TODO: throw these options into a Struct
    // TODO: support loading configuration from file

    let transport_scheme = app_m.value_of("transport-scheme").unwrap_or("https");
    let default_registry = app_m
        .value_of("default-registry")
        .unwrap_or("registry-1.docker.io");
    // TODO: this should probably be a more robust check
    let docker_compat = default_registry.contains("docker");

    let username = app_m.value_of("username");
    let password = app_m.value_of("password");

    let credentials = match (username, password) {
        (Some(user), Some(pass)) => Credentials::UserPass(user.to_string(), pass.to_string()),
        _ => Credentials::Anonymous,
    };

    match app_m.subcommand() {
        ("raw", Some(app_m)) => {
            match app_m.subcommand() {
                ("catalog", Some(sub_m)) => {
                    let init_paginate = match sub_m.value_of("n") {
                        Some(n) => Some(Paginate::new(n.parse()?, "".to_string())),
                        None => None,
                    };

                    let client = Client::new(transport_scheme, default_registry, credentials)?;

                    let mut paginate = init_paginate;
                    loop {
                        let (catalog, next_paginate) =
                            match client.get_raw_catalog(paginate).await? {
                                Some((catalog, next_paginate)) => (catalog, next_paginate),
                                None => {
                                    eprintln!("Registry doesn't support the _catalog endpoint");
                                    break;
                                }
                            };

                        std::io::stdout().write_all(&catalog)?;

                        if next_paginate.is_none() {
                            break;
                        }

                        // quick and dirty "wait for enter" paging
                        let _ = std::io::stdin().bytes().next();

                        paginate = next_paginate;
                    }
                }
                ("tags", Some(sub_m)) => {
                    // won't panic, as this is a required argument
                    let repo = sub_m.value_of("repo").unwrap();
                    let init_paginate = match sub_m.value_of("n") {
                        Some(n) => Some(Paginate::new(n.parse()?, "".to_string())),
                        None => None,
                    };

                    let image = Reference::parse(repo, default_registry, docker_compat)?;

                    let client = Client::new(transport_scheme, default_registry, credentials)?;

                    let mut paginate = init_paginate;
                    loop {
                        let (tags, next_paginate) =
                            client.get_raw_tags(image.repo(), paginate).await?;

                        std::io::stdout().write_all(&tags)?;

                        if next_paginate.is_none() {
                            break;
                        }

                        // quick and dirty "wait for enter" paging
                        let _ = std::io::stdin().bytes().next();

                        paginate = next_paginate;
                    }
                }
                ("manifest", Some(sub_m)) => {
                    // won't panic, as these are required arguments
                    let image = sub_m.value_of("image").unwrap();

                    let image = Reference::parse(image, default_registry, docker_compat)?;
                    eprintln!("canonical: {:#?}", image);

                    let client = Client::new(transport_scheme, image.registry(), credentials)?;

                    let progress = ProgressBar::new(0);
                    progress.set_style(PB_STYLE.clone());
                    progress.set_message("manifest.json");

                    let (mut manifest, digest) = client.get_raw_manifest(&image).await?;
                    progress.println(format!("Server reported digest: {}", digest));

                    progress.set_length(manifest.len().unwrap_or(0));

                    // dump the blob to stdout, chunk by chunk
                    let mut stdout = tokio::io::stdout();
                    while let Some(data) = manifest.chunk().await? {
                        let bytes_written = stdout.write(data.as_ref()).await?;
                        progress.inc(bytes_written as u64);
                    }
                    progress.finish();
                }
                ("blob", Some(sub_m)) => {
                    // won't panic, as these are required arguments
                    let repo_digest = sub_m.value_of("repo@digest").unwrap();

                    let image = Reference::parse(repo_digest, default_registry, docker_compat)?;
                    eprintln!("canonical: {:#?}", image);

                    let digest = match image.kind() {
                        ReferenceKind::Digest(digest) => digest,
                        _ => return Err(failure::err_msg("must specify digest")),
                    };

                    let client = Client::new(transport_scheme, default_registry, credentials)?;

                    let progress = ProgressBar::new(0);
                    progress.set_style(PB_STYLE.clone());
                    progress.set_message(&digest.split(':').nth(1).unwrap()[..16]);

                    let mut blob = match sub_m.value_of("range") {
                        Some(s) => {
                            let range: ParsableRange<u64> = s.parse()?;
                            client
                                .get_raw_blob_part(image.repo(), digest, range)
                                .await?
                        }
                        None => client.get_raw_blob(image.repo(), digest).await?,
                    };

                    progress.set_length(blob.len().unwrap_or(0));

                    // dump the blob to stdout, chunk by chunk
                    let mut stdout = tokio::io::stdout();
                    while let Some(data) = blob.chunk().await? {
                        let bytes_written = stdout.write(data.as_ref()).await?;
                        progress.inc(bytes_written as u64);
                    }
                    progress.finish();
                }
                _ => unreachable!(),
            }
        }
        ("download", Some(sub_m)) => {
            // won't panic, as these are required arguments
            let outdir = sub_m.value_of("outdir").unwrap();
            let image = sub_m.value_of("image").unwrap();

            let start_time = Instant::now();

            let image = Reference::parse(image, default_registry, docker_compat)?;
            eprintln!("canonical: {:#?}", image);

            let client = Client::new(transport_scheme, image.registry(), credentials)?;

            let overall_progress = MultiProgress::new();

            let manifest_progress = overall_progress.add(ProgressBar::new(0));
            manifest_progress.set_message("manifest.json");
            manifest_progress.set_style(PB_STYLE.clone());

            let config_progress = overall_progress.add(ProgressBar::new(0));
            config_progress.set_message("config.json");
            config_progress.set_style(PB_STYLE.clone());

            let containrs::flows::ImageDownload {
                manifest_digest,
                manifest_json,
                config_json,
                layers,
            } = containrs::flows::download_image(&client, &image).await?;

            eprintln!("flows::download_image() time: {:?}", start_time.elapsed());

            // asyncronously dump the bodies to disk

            lazy_static! {
                static ref MEDIA_TYPE_TO_FILE_EXT: HashMap<&'static str, &'static str> = {
                    use ociv1::media_type::*;
                    let mut m: HashMap<&str, &str> = HashMap::new();
                    m.insert(IMAGE_LAYER, "tar");
                    m.insert(IMAGE_LAYER_GZIP, "tar.gz");
                    m.insert(IMAGE_LAYER_GZIP_DOCKER, "tar.gz");
                    m.insert(IMAGE_LAYER_ZSTD, "tar.zst");
                    m.insert(IMAGE_LAYER_NON_DISTRIBUTABLE, "tar");
                    m.insert(IMAGE_LAYER_NON_DISTRIBUTABLE_GZIP, "tar.gz");
                    m.insert(IMAGE_LAYER_NON_DISTRIBUTABLE_ZSTD, "tar.zst");
                    m
                };
            }

            let out_dir = Path::new(outdir);

            if !out_dir.exists() {
                return Err(failure::err_msg("outdir does not exist"));
            }

            // create an output directory based on the manifest's digest
            let out_dir = out_dir.join(manifest_digest.replace(':', "-"));
            fs::create_dir(&out_dir)
                .await
                .context(format!("{:?}", out_dir))
                .context("failed to create directory")?;

            let mut downloads = Vec::new();

            // dump manifest.json to disk
            manifest_progress.set_length(manifest_json.len().unwrap_or(0));
            downloads.push(write_blob_to_file(
                out_dir.join("manifest.json"),
                manifest_json,
                manifest_progress,
            ));

            // dump config.json to disk
            config_progress.set_length(config_json.len().unwrap_or(0));
            downloads.push(write_blob_to_file(
                out_dir.join("config.json"),
                config_json,
                config_progress,
            ));

            // dump layers to disk
            downloads.extend(layers.into_iter().map(|(blob, layer)| {
                let filename = format!(
                    "{}.{}",
                    layer.digest.replace(':', "-"),
                    MEDIA_TYPE_TO_FILE_EXT
                        .get(layer.media_type.as_str())
                        .unwrap_or(&"unknown")
                );

                let layer_progress =
                    overall_progress.add(ProgressBar::new(blob.len().unwrap_or(0)));
                layer_progress.set_message(&layer.digest.split(':').nth(1).unwrap()[..16]);
                layer_progress.set_style(PB_STYLE.clone());
                write_blob_to_file(out_dir.join(filename), blob, layer_progress)
            }));

            let progress_handle = std::thread::spawn(move || {
                let _ = overall_progress.join();
            });

            future::try_join_all(downloads).await?;
            let _ = progress_handle.join();

            eprintln!("Full download flow time: {:?}", start_time.elapsed());

            // TODO: validate downloaded data (JSON structure, digests, etc...)
        }
        _ => unreachable!(),
    }

    Ok(())
}

async fn write_blob_to_file(
    file_path: PathBuf,
    mut blob: Blob,
    progress: ProgressBar,
) -> Result<(), failure::Error> {
    let mut file = File::create(&file_path)
        .await
        .context(format!("could not create {:?}", file_path))?;

    while let Some(data) = blob
        .chunk()
        .await
        .context(format!("error while downloading {:?}", file_path))?
    {
        let bytes_written = file
            .write(data.as_ref())
            .await
            .context(format!("error while writing to {:?}", file_path))?;
        progress.inc(bytes_written as u64);
    }
    progress.finish();
    Ok(())
}