use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::{fs, io};

use cargo_toml::{DepsSet, Manifest};
use cargo_toml_builder::CargoToml;
use common::project_description_dto::target_kind::TargetKind;
use common::project_description_dto::ProjectDescriptionDto;
use thiserror::Error;

use crate::cargo_toml_parser_extensions::traits::{Combine, MyToString};
use crate::service::project::{Project, ProjectFileTarget};
use crate::service::project_generation_service::ProjectGeneratingServiceError::DependencySection;
use crate::service::starter_service::StarterService;
use crate::service::{compressor, project};
use crate::storage::traits::MapStorage;
use crate::{hash, push};

const MAIN: &str = r#"fn main() {
    println!("Hello, world!");
}"#;

const LIB: &str = r#"pub fn add(left: usize, right: usize) -> usize {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
"#;

#[derive(Error, Debug)]
pub enum ProjectGeneratingServiceError {
    #[error("Could not generate description section: {0:?}")]
    DescriptionSection(#[from] cargo_toml_builder::Error),

    #[error("Could not generate dependency section: {0:?}")]
    DependencySection(String),

    #[error("Could not get content of '{0:?}' starter")]
    CouldNotGetStarterContent(String),

    #[error("Could not parse starter manifest")]
    CouldNotParseStarterManifest(#[from] cargo_toml::Error),

    #[error("Could not compress project: {0:?}")]
    CompressionError(#[from] compressor::CompressingError),

    #[error("Could not generate project because of IO error: {0:?}")]
    IoError(#[from] io::Error),

    #[error("Could not generate project: {0:?}")]
    ProjectError(#[from] project::ProjectError),
}

pub async fn generate<T: MapStorage>(
    description_dto: &ProjectDescriptionDto,
    starter_service: &StarterService<T>,
) -> Result<Vec<u8>, ProjectGeneratingServiceError> {
    let project_hash = get_project_hash(description_dto);
    let mut empty_project = Project::new(
        &project_hash,
        &description_dto.package_description.name,
        &description_dto.target_kind,
    )?;

    match description_dto.target_kind {
        TargetKind::Bin => empty_project.write_to_file(MAIN, ProjectFileTarget::Main)?,
        TargetKind::Lib => empty_project.write_to_file(LIB, ProjectFileTarget::Lib)?,
    }

    let label = format!("{}{}", '#', dotenv::var("LABEL").unwrap());
    let package_section = generate_package_section(description_dto)?;

    if description_dto.starters.is_empty() {
        empty_project.write_to_file(
            &format!("{}{}", label, package_section),
            ProjectFileTarget::Cargo,
        )?;

        let zipped_project = compressor::zip_project(
            empty_project.get_hashed_dir_path(),
            &description_dto.package_description.name,
            zip::CompressionMethod::Deflated,
        )?;

        return Ok(fs::read(zipped_project)?);
    }

    let dependency_section = generate_dependency_section(description_dto, starter_service).await?;
    let cargo_file_content = format!("{}{}{}", label, package_section, dependency_section);
    empty_project.write_to_file(&cargo_file_content, ProjectFileTarget::Cargo)?;

    let zipped_project = compressor::zip_project(
        empty_project.get_hashed_dir_path(),
        &description_dto.package_description.name,
        zip::CompressionMethod::Deflated,
    )?;

    Ok(fs::read(zipped_project)?)
}

async fn generate_dependency_section<T: MapStorage>(
    description_dto: &ProjectDescriptionDto,
    starter_service: &StarterService<T>,
) -> Result<String, ProjectGeneratingServiceError> {
    let mut starter_names = Vec::with_capacity(description_dto.starters.len());

    for starter in &description_dto.starters {
        let starter_content = starter_service
            .get_starter_by_name(starter)
            .await
            .map_err(|e| ProjectGeneratingServiceError::CouldNotGetStarterContent(e.to_string()))?
            .raw_starter
            .0;
        starter_names.push(starter_content);
    }

    let mut parsed_starters = Vec::with_capacity(starter_names.len());

    for starter in starter_names {
        let starter_content = Manifest::from_str(&starter)?;
        parsed_starters.push(starter_content);
    }

    let mut dependency_section = String::new();

    let dependency_set = parsed_starters
        .into_iter()
        .map(|manifest| manifest.dependencies)
        .try_fold(DepsSet::new(), proceed_starters)?;

    for dependency in &dependency_set {
        dependency_section.push_str(
            &dependency
                .to_string()
                .map_err(|e| DependencySection(e.to_string()))?,
        );
        dependency_section.push('\n');
    }

    Ok(dependency_section)
}

fn proceed_starters(
    mut starter_a: DepsSet,
    mut starter_b: DepsSet,
) -> Result<DepsSet, ProjectGeneratingServiceError> {
    let mut result = DepsSet::new();

    let dependency_set_a = starter_a.clone().into_keys().collect::<HashSet<String>>();
    let dependency_set_b = starter_b.clone().into_keys().collect::<HashSet<String>>();
    let symmetric_difference = dependency_set_a
        .symmetric_difference(&dependency_set_b)
        .cloned()
        .collect::<Vec<String>>();
    let intersection = dependency_set_a
        .intersection(&dependency_set_b)
        .cloned()
        .collect::<Vec<String>>();

    for dep_name in &intersection {
        let dep_a = starter_a
            .get(dep_name)
            .ok_or_else(|| DependencySection("Could not get dependency".into()))?;
        let dep_b = starter_b
            .get(dep_name)
            .ok_or_else(|| DependencySection("Could not get dependency".into()))?;

        let final_dep = dep_a
            .combine_dependencies(dep_b)
            .map_err(|e| DependencySection(e.to_string()))?;
        result.insert(dep_name.clone(), final_dep);
    }

    starter_a.append(&mut starter_b);

    for dep_name in &symmetric_difference {
        let key_value = starter_a
            .get_key_value(dep_name)
            .ok_or_else(|| DependencySection("Could not get dependency".into()))?;
        result.insert(key_value.0.clone(), key_value.1.clone());
    }

    Ok(result)
}

fn generate_package_section(
    description_dto: &ProjectDescriptionDto,
) -> Result<String, ProjectGeneratingServiceError> {
    Ok(CargoToml::builder()
        .name(&description_dto.package_description.name)
        .version("0.1.0")
        .description(
            &description_dto
                .package_description
                .description
                .clone()
                .unwrap_or_else(|| "No description".to_owned()),
        )
        .author(
            &description_dto
                .package_description
                .author
                .clone()
                .unwrap_or_else(|| "Unspecified Author".to_owned()),
        )
        .build()?
        .to_string())
}

fn get_project_hash(description_dto: &ProjectDescriptionDto) -> String {
    let current_time = chrono::Utc::now().to_string();

    let mut project_hash = hash!(format!("{}{}", current_time, hash!(description_dto))).to_string();

    tracing::info!("Project hash: {}", project_hash);

    while is_project_hash_exists(&project_hash) {
        project_hash = get_project_hash(description_dto);
    }

    project_hash
}

fn is_project_hash_exists(project_hash: &str) -> bool {
    push!(PathBuf::from(dotenv::var("TEMP").unwrap()), project_hash).exists()
}
