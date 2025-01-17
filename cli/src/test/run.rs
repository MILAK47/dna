use std::fs::File;
use std::io::{BufWriter, Write};

use std::{fs, path::Path};

use tracing::warn;
use walkdir::{DirEntry, WalkDir};

use apibara_sink_common::{
    load_environment_variables, load_script, DotenvOptions, NetworkFilterOptions,
    OptionsFromScript, ScriptOptions, StreamConfigurationOptions, StreamOptions,
};
use colored::*;
use error_stack::{Result, ResultExt};
use similar_asserts::serde_impl::Debug as SimilarAssertsDebug;
use similar_asserts::SimpleDiff;

use crate::error::CliError;
use crate::test::error::get_assertion_error;
use crate::test::snapshot::{Snapshot, SnapshotGenerator};

const DEFAULT_NUM_BATCHES: usize = 1;

fn to_relative_path(path: &Path) -> &Path {
    let current_dir = std::env::current_dir().unwrap();
    if let Ok(stripped) = path.strip_prefix(&current_dir) {
        stripped
    } else {
        path
    }
}

#[derive(Debug)]
pub enum TestResult {
    Passed,
    Failed { message: String },
}

pub async fn run_single_test(
    snapshot_path: &Path,
    snapshot: Option<Snapshot>,
    script_path: Option<&Path>,
    dotenv_options: &DotenvOptions,
) -> Result<TestResult, CliError> {
    let snapshot_path_display = to_relative_path(snapshot_path).display();

    println!(
        "{} test `{}` ... ",
        "Running".green().bold(),
        snapshot_path_display
    );

    let snapshot = if let Some(snapshot) = snapshot {
        snapshot
    } else {
        let file = fs::File::open(snapshot_path)
            .change_context(CliError)
            .attach_printable_lazy(|| {
                format!("Cannot open snapshot file `{}`", snapshot_path_display)
            })?;

        let snapshot: Snapshot = serde_json::from_reader(file)
            .change_context(CliError)
            .attach_printable_lazy(|| {
                format!(
                    "Cannot decode json file as a Snapshot `{}`",
                    snapshot_path_display
                )
            })?;
        snapshot
    };

    run_test(snapshot, script_path, dotenv_options).await
}

async fn run_test(
    snapshot: Snapshot,
    script_path: Option<&Path>,
    dotenv_options: &DotenvOptions,
) -> Result<TestResult, CliError> {
    let hint =
        "rerun with --override to regenerate the snapshot or change the snapshot name with --name";

    if let Some(script_path) = script_path {
        if snapshot.script_path != script_path {
            let message = format!(
                "Snapshot generated with a different script: `{}`, {}",
                snapshot.script_path.display(),
                hint
            );
            return Ok(TestResult::Failed { message });
        }
    }

    let script_path_str = snapshot.script_path.to_string_lossy().to_string();
    let allow_env = load_environment_variables(dotenv_options).change_context(CliError)?;
    let mut script =
        load_script(&script_path_str, ScriptOptions { allow_env }).change_context(CliError)?;

    let filter = &script
        .configuration::<OptionsFromScript>()
        .await
        .change_context(CliError)?
        .stream_configuration
        .as_starknet()
        .ok_or(CliError)
        .attach_printable("Cannot convert StreamConfigurationOptions using as_starknet")?
        .filter;

    let NetworkFilterOptions::Starknet(snapshot_filter) =
        &snapshot.stream_configuration_options.filter;

    if snapshot_filter != filter {
        let left = format!("{:#?}", SimilarAssertsDebug(&snapshot_filter));
        let right = format!("{:#?}", SimilarAssertsDebug(&filter));

        let diff = SimpleDiff::from_str(left.as_str(), right.as_str(), "expected", "found");

        let message = format!(
            "Snapshot generated with a different filter, {}\n{}",
            hint, &diff
        );
        return Ok(TestResult::Failed { message });
    }

    let mut expected_outputs = vec![];
    let mut found_outputs = vec![];

    for message in snapshot.stream {
        let input = message["input"]
            .as_array()
            .ok_or(CliError)
            .attach_printable("snapshot input should be an array")?
            .clone();
        let expected_output = message["output"].clone();

        let found_output = script
            .transform(input)
            .await
            .change_context(CliError)
            .attach_printable("failed to transform data")?;

        expected_outputs.push(expected_output.clone());
        found_outputs.push(found_output.clone());
    }

    if expected_outputs != found_outputs {
        let message = get_assertion_error(&expected_outputs, &found_outputs);
        Ok(TestResult::Failed { message })
    } else {
        Ok(TestResult::Passed)
    }
}

/// Merge stream_options and stream_configuration_options from CLI, script and
/// snapshot if it exists
/// Priority: CLI > snapshot > script except for filter which is exclusively configured from script
pub async fn merge_options(
    starting_block: Option<u64>,
    num_batches: Option<usize>,
    cli_stream_options: &StreamOptions,
    script_options: OptionsFromScript,
    snapshot: Option<Snapshot>,
) -> Result<(StreamOptions, StreamConfigurationOptions, usize), CliError> {
    if let Some(snapshot) = snapshot {
        let stream_options = cli_stream_options
            .clone()
            .merge(snapshot.stream_options)
            .merge(script_options.stream);

        let mut stream_configuration_options = snapshot
            .stream_configuration_options
            .merge(script_options.stream_configuration.clone());

        stream_configuration_options.starting_block =
            starting_block.or(stream_configuration_options.starting_block);

        stream_configuration_options.filter = script_options.stream_configuration.filter;

        let num_batches = num_batches.unwrap_or(snapshot.num_batches);

        Ok((stream_options, stream_configuration_options, num_batches))
    } else {
        let stream_options = cli_stream_options.clone().merge(script_options.stream);

        let mut stream_configuration_options = script_options.stream_configuration;

        stream_configuration_options.starting_block =
            starting_block.or(stream_configuration_options.starting_block);

        let num_batches = num_batches.unwrap_or(DEFAULT_NUM_BATCHES);

        Ok((stream_options, stream_configuration_options, num_batches))
    }
}

pub async fn run_generate_snapshot(
    script_path: &Path,
    snapshot_path: &Path,
    starting_block: Option<u64>,
    num_batches: Option<usize>,
    cli_stream_options: &StreamOptions,
    dotenv_options: &DotenvOptions,
) -> Result<(), CliError> {
    println!(
        "{} snapshot `{}` ...",
        "Generating".green().bold(),
        to_relative_path(snapshot_path).display()
    );

    let script_path_str = script_path.to_string_lossy().to_string();
    let allow_env = load_environment_variables(dotenv_options).change_context(CliError)?;
    let mut script =
        load_script(&script_path_str, ScriptOptions { allow_env }).change_context(CliError)?;

    let script_options = script
        .configuration::<OptionsFromScript>()
        .await
        .change_context(CliError)?;

    let snapshot = if snapshot_path.exists() {
        match fs::File::open(snapshot_path) {
            Ok(file) => serde_json::from_reader(file).ok(),
            Err(err) => {
                warn!(err =? err, "Cannot read snapshot file to get previously used options `{}`", snapshot_path.display());
                None
            }
        }
    } else {
        None
    };

    let (stream_options, stream_configuration_options, num_batches) = merge_options(
        starting_block,
        num_batches,
        cli_stream_options,
        script_options,
        snapshot,
    )
    .await?;

    let snapshot = SnapshotGenerator::new(
        script_path.to_owned(),
        script,
        num_batches,
        stream_options,
        stream_configuration_options,
    )
    .generate()
    .await?;

    if !&snapshot_path.parent().unwrap().exists() {
        fs::create_dir_all(snapshot_path.parent().unwrap()).change_context(CliError)?;
    }

    let file = File::create(snapshot_path).change_context(CliError)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, &snapshot).change_context(CliError)?;
    writer.flush().change_context(CliError)?;

    let start_block = snapshot.stream[0]["cursor"]["orderKey"]
        .as_u64()
        .unwrap_or(0);
    let end_block = &snapshot.stream.last().unwrap()["end_cursor"]["orderKey"]
        .as_u64()
        .unwrap();

    let num_batches = snapshot.stream.len();
    let num_batches = if num_batches > 1 {
        format!("{} batches ({} -> {})", num_batches, start_block, end_block)
    } else {
        format!("{} batch ({} -> {})", num_batches, start_block, end_block)
    };

    println!(
        "{} snapshot successfully with {}",
        "Generated".green().bold(),
        num_batches.green().bold(),
    );

    Ok(())
}

pub async fn run_all_tests(
    dir: impl AsRef<Path>,
    dotenv_options: &DotenvOptions,
    script_path: Option<&Path>,
) -> Result<(), CliError> {
    let for_script = if let Some(script_path) = script_path {
        format!(" for `{}`", to_relative_path(script_path).display())
    } else {
        "".to_string()
    };

    println!(
        "{} tests{} from `{}` ... ",
        "Collecting".green().bold(),
        for_script,
        to_relative_path(dir.as_ref()).display(),
    );

    let snapshots: Vec<(DirEntry, Option<Snapshot>)> = WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|e| e == "json").unwrap_or(false))
        .filter_map(|e| {
            if let Some(script_path) = script_path {
                let file = fs::File::open(e.path());
                match file {
                    Ok(file) => {
                        let snapshot: std::result::Result<Snapshot, serde_json::Error> =
                            serde_json::from_reader(file);

                        match snapshot {
                            Ok(snapshot) => {
                                if snapshot.script_path == script_path {
                                    Some((e, Some(snapshot)))
                                } else {
                                    None
                                }
                            }
                            Err(err) => {
                                warn!(err =? err, "Cannot decode json file as a Sanpshot `{}`", e.path().display());
                                None
                            }
                        }
                    }
                    Err(err) => {
                        warn!(err =? err, "Cannot open snapshot file `{}`", e.path().display());
                        None
                    }
                }
            } else {
                Some((e, None))
            }
        })
        .collect();

    println!("{} {} files", "Collected".green().bold(), snapshots.len());

    let mut num_passed_tests = 0;
    let mut num_failed_tests = 0;
    let mut num_error_tests = 0;

    for (snapshot_path, snapshot) in snapshots {
        println!();
        match run_single_test(snapshot_path.path(), snapshot, None, dotenv_options).await {
            Ok(TestResult::Passed) => {
                println!("{}", "Test passed".green());
                num_passed_tests += 1;
            }
            Ok(TestResult::Failed { message }) => {
                println!("{}\n", "Test failed".red());
                eprintln!("{}", message);
                num_failed_tests += 1;
            }
            Err(err) => {
                println!("{}\n", "Test error".red());
                eprintln!("{}", format!("{err:?}").bright_red());
                num_error_tests += 1
            }
        };
    }

    let passed = format!("{} passed", num_passed_tests).green();
    let failed = format!("{} failed", num_failed_tests).red();
    let error = format!("{} error", num_error_tests).bright_red();

    println!(
        "\n{}: {}, {}, {}",
        "Test result".bold(),
        passed,
        failed,
        error
    );

    Ok(())
}
