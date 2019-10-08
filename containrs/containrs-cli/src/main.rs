#![allow(clippy::cognitive_complexity)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Instant;

use clap::{App, AppSettings, Arg, SubCommand};
use failure::ResultExt;
use futures::future;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use lazy_static::lazy_static;
use tokio::fs::{self, File};
use tokio::prelude::*;

use docker_reference::{Reference, ReferenceKind};
use oci_digest::Digest;
use oci_image::v1 as ociv1;

use containrs::{Blob, Client, Credentials, Paginate};

mod parse_range;
use crate::parse_range::ParsableRange;

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
        .progress_chars("=>-");
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
                .arg(
                    Arg::with_name("skip-validate")
                        .help("Skip validating downloaded image digests")
                        .long("skip-validate")
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
                    let repo = sub_m
                        .value_of("repo")
                        .expect("repo should be a required argument");
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
                    let image = sub_m
                        .value_of("image")
                        .expect("image should be a required argument");

                    let image = Reference::parse(image, default_registry, docker_compat)?;
                    eprintln!("canonical: {:#?}", image);

                    let client = Client::new(transport_scheme, image.registry(), credentials)?;

                    let progress = ProgressBar::new(0);
                    progress.set_style(PB_STYLE.clone());
                    progress.set_message("manifest.json");

                    let mut manifest = client.get_raw_manifest(&image).await?;
                    progress.println(format!(
                        "Server reported digest: {}",
                        manifest.get_expected_digest()
                    ));

                    progress.set_length(manifest.len().unwrap_or(0));

                    // dump the blob to stdout, chunk by chunk
                    let mut validator = manifest
                        .get_expected_digest()
                        .validator()
                        .ok_or_else(|| failure::err_msg("unsupported digest algorithm"))?;
                    let mut stdout = tokio::io::stdout();
                    while let Some(data) = manifest.chunk().await? {
                        validator.input(&data);
                        let bytes_written = stdout.write(data.as_ref()).await?;
                        progress.inc(bytes_written as u64);
                    }
                    progress.finish();

                    eprintln!(
                        "Calculated digest {} the expected digest",
                        if validator.validate() {
                            "matches"
                        } else {
                            "**does not** match"
                        }
                    );
                }
                ("blob", Some(sub_m)) => {
                    let repo_digest = sub_m
                        .value_of("repo@digest")
                        .expect("repo@digest should be a required argument");

                    let image = Reference::parse(repo_digest, default_registry, docker_compat)?;
                    eprintln!("canonical: {:#?}", image);

                    let digest = match image.kind() {
                        ReferenceKind::Digest(digest) => digest,
                        _ => return Err(failure::err_msg("must specify digest")),
                    };

                    let client = Client::new(transport_scheme, default_registry, credentials)?;

                    let progress = ProgressBar::new(0);
                    progress.set_style(PB_STYLE.clone());
                    progress.set_message(&digest.as_str().split(':').nth(1).unwrap()[..16]);

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

                    // dump the blob to stdout, chunk by chunk, validating it along the way
                    let mut validator = digest
                        .validator()
                        .ok_or_else(|| failure::err_msg("unsupported digest algorithm"))?;
                    let mut stdout = tokio::io::stdout();
                    while let Some(data) = blob.chunk().await? {
                        validator.input(&data);
                        let bytes_written = stdout.write(data.as_ref()).await?;
                        progress.inc(bytes_written as u64);
                    }
                    progress.finish();

                    eprintln!(
                        "Calculated digest {} the expected digest",
                        if validator.validate() {
                            "matches"
                        } else {
                            "**does not** match"
                        }
                    );
                }
                _ => unreachable!(),
            }
        }
        ("download", Some(sub_m)) => {
            let outdir = sub_m
                .value_of("outdir")
                .expect("outdir should be a required argument");
            let image = sub_m
                .value_of("image")
                .expect("image should be a required argument");
            let skip_validate = sub_m.is_present("skip-validate");

            let out_dir = Path::new(outdir);
            if !out_dir.exists() {
                return Err(failure::err_msg("outdir does not exist"));
            }

            // parse image reference
            let image = Reference::parse(image, default_registry, docker_compat)?;
            eprintln!("canonical: {:#?}", image);

            // setup client
            let client = Client::new(transport_scheme, image.registry(), credentials)?;

            let download_timer = Instant::now();

            // fetch manifest
            let manifest_blob = client.get_raw_manifest(&image).await?;
            eprintln!("downloading manifest.json...");
            let manifest_digest = manifest_blob.get_expected_digest().clone();
            let manifest_json = manifest_blob.bytes().await?;
            eprintln!("downloaded manifest.json");

            // validate manifest
            if !manifest_digest.validate(&manifest_json) {
                return Err(failure::err_msg("manifest.json could not be validated"));
            } else {
                eprintln!("manifest.json validated");
            }

            // create an output directory based on the manifest's digest
            let out_dir = out_dir.join(manifest_digest.as_str().replace(':', "-"));
            fs::create_dir(&out_dir)
                .await
                .context(format!("{:?}", out_dir))
                .context("failed to create directory")?;

            // dump manifest.json to disk
            fs::write(out_dir.join("manifest.json"), &manifest_json).await?;

            // parse and validate the syntax of the manifest.json file
            let manifest = serde_json::from_slice::<ociv1::Manifest>(&manifest_json)
                .context("while parsing manifest.json")?;

            eprintln!("firing off download requests...");

            // Use the parsed manifest to build up a list of blobs to download.
            let mut paths = Vec::new();
            let mut digests = Vec::new();

            paths.push(String::from("config.json"));
            digests.push(&manifest.config.digest);

            for layer in manifest.layers.iter() {
                let filename = format!(
                    "{}.{}",
                    layer.digest.as_str().replace(':', "-"),
                    MEDIA_TYPE_TO_FILE_EXT
                        .get(layer.media_type.as_str())
                        .unwrap_or(&"unknown")
                );

                paths.push(filename);
                digests.push(&layer.digest);
            }

            let paths = paths
                .into_iter()
                .map(|file| out_dir.join(file))
                .collect::<Vec<_>>();

            // fire off downloads in parallel
            let blob_futures = digests
                .iter()
                .map(|digest| client.get_raw_blob(image.repo(), &digest));

            // TODO: there's no need to artificially wait for all the futures to kick off.
            // This checkpoint is only here for benchmarking how well the auth header cache
            // performs.
            //
            // For maximum throughput, these futures should all immediately chain with
            // write_blob_to_file.
            let blobs = future::try_join_all(blob_futures).await?;
            eprintln!(
                "fired off all layer download requests in {:?}",
                download_timer.elapsed()
            );

            // asynchronously download and dump the blobs to disk
            let download_progress = MultiProgress::new();

            let downloads = blobs
                .into_iter()
                .zip(paths.iter())
                .map(|(blob, path)| {
                    let layer_progress = download_progress.add(ProgressBar::new(0));
                    layer_progress.set_message(&{
                        let mut msg = path
                            .file_name()
                            .unwrap()
                            .to_os_string()
                            .into_string()
                            .unwrap();
                        msg.truncate(16);
                        msg
                    });
                    layer_progress.set_style(PB_STYLE.clone());
                    layer_progress.set_length(blob.len().unwrap_or(0));

                    write_blob_to_file(&path, blob, layer_progress)
                })
                .collect::<Vec<_>>();

            let progress_handle = std::thread::spawn(move || {
                let _ = download_progress.join();
            });

            future::try_join_all(downloads).await?;
            let _ = progress_handle.join();

            eprintln!("full download flow time: {:?}", download_timer.elapsed());

            if skip_validate {
                return Ok(());
            }

            eprintln!("validating files...");

            let validation_progress = MultiProgress::new();
            let validate_progress = validation_progress.add(ProgressBar::new(paths.len() as u64));
            validate_progress.set_style(
                ProgressStyle::default_bar()
                    .template("[{elapsed_precise}] {pos}/{len} files validated"),
            );
            validate_progress.enable_steady_tick(1000);

            let validations = paths
                .iter()
                .zip(digests.iter())
                .map(|(path, digest)| validate_file(path, digest, &validate_progress))
                .collect::<Vec<_>>();

            let progress_handle = std::thread::spawn(move || {
                let _ = validation_progress.join();
            });

            future::try_join_all(validations).await?;
            validate_progress.finish();
            let _ = progress_handle.join();
            eprintln!("all files validated correctly");
        }
        _ => unreachable!(),
    }

    Ok(())
}

/// Writes a blob to disk as it's downloaded
async fn write_blob_to_file(
    file_path: &Path,
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

/// Reads a file from disk, and validates it with the given digest
async fn validate_file(
    file_path: &Path,
    digest: &Digest,
    progress: &ProgressBar,
) -> Result<(), failure::Error> {
    let mut validator = digest
        .validator()
        .ok_or_else(|| failure::err_msg("unsupported digest algorithm"))?;

    let mut file = File::open(&file_path)
        .await
        .context(format!("could not open {:?}", file_path))?;

    let mut buf = [0; 2048];
    loop {
        let len = file.read(&mut buf).await?;
        if len == 0 {
            progress.inc(1);
            if validator.validate() {
                return Ok(());
            } else {
                return Err(failure::err_msg(format!(
                    "Digest mismatch! {:?}",
                    file_path.file_name().unwrap()
                )));
            }
        }
        validator.input(&buf[..len]);
    }
}
