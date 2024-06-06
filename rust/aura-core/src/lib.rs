//! Core package manager functionality that doesn't assume a certain frontend,
//! logging framework, or Error stack.

#![warn(missing_docs)]

pub mod aur;
pub mod cache;
pub mod deps;
pub mod faur;
pub mod git;
pub mod log;
pub mod snapshot;

use alpm::{AlpmList, Db, PackageReason, SigLevel};
use alpm_utils::DbListExt;
use r2d2_alpm::Alpm;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::fs::DirEntry;
use std::path::Path;
use walkdir::WalkDir;

/// Types that act like a package database.
pub trait DbLike {
    /// A simple package lookup.
    fn get_pkg<'a, S>(&'a self, name: S) -> Result<&'a alpm::Package, alpm::Error>
    where
        S: Into<Vec<u8>>;

    /// Find a package that provides some name.
    fn provides<'a, S>(&'a self, name: S) -> Option<&'a alpm::Package>
    where
        S: Into<Vec<u8>>;
}

impl DbLike for Db {
    fn get_pkg<'a, S>(&'a self, name: S) -> Result<&'a alpm::Package, alpm::Error>
    where
        S: Into<Vec<u8>>,
    {
        self.pkg(name)
    }

    fn provides<'a, S>(&'a self, _: S) -> Option<&'a alpm::Package>
    where
        S: Into<Vec<u8>>,
    {
        None
    }
}

impl DbLike for AlpmList<'_, &Db> {
    fn get_pkg<'a, S>(&'a self, name: S) -> Result<&'a alpm::Package, alpm::Error>
    where
        S: Into<Vec<u8>>,
    {
        self.pkg(name)
    }

    fn provides<'a, S>(&'a self, name: S) -> Option<&'a alpm::Package>
    where
        S: Into<Vec<u8>>,
    {
        self.find_satisfier(name)
    }
}

/// A combination of both database sources.
pub struct Dbs<'a, 'b> {
    local: &'a Db,
    syncs: AlpmList<'b, &'a Db>,
}

impl<'a, 'b> Dbs<'a, 'b> {
    /// Form a combination of all available package databases.
    pub fn from_alpm(alpm: &'a Alpm) -> Dbs<'a, 'a> {
        Dbs {
            local: alpm.as_ref().localdb(),
            syncs: alpm.as_ref().syncdbs(),
        }
    }
}

impl<'b, 'c> DbLike for Dbs<'b, 'c> {
    fn get_pkg<'a, S>(&'a self, name: S) -> Result<&'a alpm::Package, alpm::Error>
    where
        S: Into<Vec<u8>>,
    {
        let v = name.into();

        self.local
            // FIXME 2024-06-07 Unfortunate clone.
            .get_pkg(v.clone())
            .or_else(|_| self.syncs.get_pkg(v))
    }

    fn provides<'a, S>(&'a self, name: S) -> Option<&'a alpm::Package>
    where
        S: Into<Vec<u8>>,
    {
        self.syncs.find_satisfier(name)
    }
}

/// The simplest form a package.
#[derive(Debug, PartialEq, Eq)]
pub struct Package<'a> {
    /// The name of the package.
    pub name: Cow<'a, str>,
    /// The version of the package.
    pub version: Cow<'a, str>,
}

impl<'a> Package<'a> {
    /// Construct a new `Package`.
    pub fn new<S, T>(name: S, version: T) -> Package<'a>
    where
        S: Into<Cow<'a, str>>,
        T: Into<Cow<'a, str>>,
    {
        Package {
            name: name.into(),
            version: version.into(),
        }
    }

    // TODO Avoid the extra String allocation.
    /// Split a [`Path`] into its package name and version.
    ///
    /// ```
    /// use aura_core::Package;
    /// use std::path::Path;
    ///
    /// let path = Path::new("/var/cache/pacman/pkg/aura-bin-3.2.1-1-x86_64.pkg.tar.zst");
    /// let pkg = Package::from_path(path).unwrap();
    /// assert_eq!("aura-bin", pkg.name);
    /// assert_eq!("3.2.1-1", pkg.version);
    ///
    /// let simple = Path::new("aura-bin-3.2.1-1-x86_64.pkg.tar.zst");
    /// let pkg = Package::from_path(simple).unwrap();
    /// assert_eq!("aura-bin", pkg.name);
    /// assert_eq!("3.2.1-1", pkg.version);
    /// ```
    pub fn from_path(path: &Path) -> Option<Package<'static>> {
        path.file_name()
            .and_then(|file| file.to_str())
            // FIXME Mon Jan 10 2022 Consider `rsplit_once` etc. here.
            .and_then(|file| file.rsplit_once('-'))
            .and_then(|(pkg, _)| {
                let mut vec: Vec<_> = pkg.rsplitn(3, '-').collect();
                let name = vec.last()?.to_string();
                vec.pop();
                vec.reverse();
                let version = vec.join("-");

                Some(Package::new(name, version))
            })
    }

    /// Does some given version string have the same value as the one in this
    /// `Package`?
    pub fn same_version(&self, other: &str) -> bool {
        match alpm::vercmp(other, &self.version) {
            Ordering::Equal => true,
            Ordering::Less | Ordering::Greater => false,
        }
    }
}

impl<'a> From<&'a alpm::Package> for Package<'a> {
    fn from(p: &'a alpm::Package) -> Self {
        Package::new(p.name(), p.version().as_str())
    }
}

impl From<crate::faur::Package> for Package<'_> {
    fn from(p: crate::faur::Package) -> Self {
        Package::new(p.name, p.version)
    }
}

impl<'a> PartialOrd for Package<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Ord for Package<'a> {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.name.cmp(&other.name) {
            Ordering::Equal => alpm::vercmp(self.version.as_ref(), other.version.as_ref()),
            otherwise => otherwise,
        }
    }
}

/// Like [`Path::read_dir`], but for multiple [`Path`]s at once.
pub fn read_dirs<P>(paths: &[P]) -> impl Iterator<Item = Result<DirEntry, std::io::Error>> + '_
where
    P: AsRef<Path>,
{
    paths
        .iter()
        .filter_map(|path| path.as_ref().read_dir().ok())
        .flatten()
}

/// Apply functions in method-position.
pub trait Apply {
    /// Apply a given function in method-position.
    fn apply<F, U>(self, f: F) -> U
    where
        F: FnOnce(Self) -> U,
        Self: Sized;
}

impl<T> Apply for T {
    fn apply<F, U>(self, f: F) -> U
    where
        F: FnOnce(Self) -> U,
        Self: Sized,
    {
        f(self)
    }
}

/// All orphaned packages.
///
/// An orphan is a package that was installed as a dependency, but whose parent
/// package is no longer installed.
pub fn orphans<A>(alpm: &A) -> impl Iterator<Item = &alpm::Package>
where
    A: AsRef<alpm::Alpm>,
{
    alpm.as_ref().localdb().pkgs().into_iter().filter(|p| {
        p.reason() == PackageReason::Depend
            && p.required_by().is_empty()
            && p.optional_for().is_empty()
    })
}

/// All packages neither required nor optionally required by any other package,
/// but are marked as explicitly installed. So in theory these are all
/// standalone applications, but occasionally some packages get installed by
/// mistake, forgotten, or mislabelled, and then just hang around on the system
/// forever, receiving pointless updates.
pub fn elderly<A>(alpm: &A) -> impl Iterator<Item = &alpm::Package>
where
    A: AsRef<alpm::Alpm>,
{
    alpm.as_ref().localdb().pkgs().into_iter().filter(|p| {
        p.reason() == PackageReason::Explicit
            && p.required_by().is_empty()
            && p.optional_for().is_empty()
    })
}

/// Does the given `Path` point to a valid tarball that can can loaded by ALPM?
pub fn is_valid_package<A>(alpm: A, path: &Path) -> bool
where
    A: AsRef<alpm::Alpm>,
{
    let sig = SigLevel::USE_DEFAULT;

    // TODO 2024-03-18 Refactor to use `is_some_and`.
    match path.to_str() {
        None => false,
        Some(p) => path.exists() && alpm.as_ref().pkg_load(p, true, sig).is_ok(),
    }
}

/// All official packages.
pub fn native_packages<A>(alpm: &A) -> impl Iterator<Item = &alpm::Package>
where
    A: AsRef<alpm::Alpm>,
{
    let syncs = alpm.as_ref().syncdbs();

    alpm.as_ref()
        .localdb()
        .pkgs()
        .into_iter()
        .filter_map(move |p| syncs.pkg(p.name()).ok())
}

/// All foreign packages as an `Iterator`.
pub fn foreign_packages<A>(alpm: &A) -> impl Iterator<Item = &alpm::Package>
where
    A: AsRef<alpm::Alpm>,
{
    let syncs = alpm.as_ref().syncdbs();

    alpm.as_ref()
        .localdb()
        .pkgs()
        .into_iter()
        .filter(move |p| syncs.pkg(p.name()).is_err())
}

/// The number of bytes contained by all files in a directory. Recursive.
pub fn recursive_dir_size<P>(path: P) -> u64
where
    P: AsRef<Path>,
{
    WalkDir::new(path)
        .into_iter()
        .filter_map(|de| de.ok())
        .filter_map(|de| de.metadata().ok())
        .map(|meta| meta.len())
        .sum()
}
