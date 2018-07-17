// Copyright Facebook, Inc. 2018
//! Python bindings for a Rust hg store
use cpython::{PyBytes, PyClone, PyDict, PyErr, PyList, PyObject, PyResult, PyString, Python,
              PythonObject};
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use pythondatastore::PythonDataStore;
use pythonutil::{from_delta_to_tuple, from_key_to_tuple, from_tuple_to_key, to_key, to_pyerr};
use revisionstore::datapack::DataPack;
use revisionstore::datastore::DataStore;
use revisionstore::key::Key;

py_module_initializer!(
    pyrevisionstore,        // module name
    initpyrevisionstore,    // py2 init name
    PyInit_pyrevisionstore, // py3 init name
    |py, m| {
        // init function
        m.add_class::<datastore>(py)?;
        m.add_class::<datapack>(py)?;
        Ok(())
    }
);

py_class!(class datastore |py| {
    data store: Box<DataStorePyExt + Send>;

    def __new__(
        _cls,
        store: &PyObject
    ) -> PyResult<datastore> {
        datastore::create_instance(
            py,
            Box::new(PythonDataStore::new(store.clone_ref(py))),
        )
    }

    def get(&self, name: &PyBytes, node: &PyBytes) -> PyResult<PyBytes> {
        self.store(py).get(py, name, node)
    }

    def getdeltachain(&self, name: &PyBytes, node: &PyBytes) -> PyResult<PyList> {
        self.store(py).get_delta_chain(py, name, node)
    }

    def getmeta(&self, name: &PyBytes, node: &PyBytes) -> PyResult<PyDict> {
        self.store(py).get_meta(py, name, node)
    }

    def getmissing(&self, keys: &PyList) -> PyResult<PyList> {
        self.store(py).get_missing(py, keys)
    }
});

py_class!(class datapack |py| {
    data store: Box<DataPack>;
    data pack_path: PathBuf;

    def __new__(
        _cls,
        path: &PyString
    ) -> PyResult<datapack> {
        let raw_str: Cow<str> = path.to_string(py)?;
        let path_str = Path::new(raw_str.as_ref());
        let path = PathBuf::from(&path_str);
        datapack::create_instance(
            py,
            Box::new(match DataPack::new(&path) {
                Ok(pack) => pack,
                Err(e) => return Err(to_pyerr(py, &e)),
            }),
            path,
        )
    }

    def path(&self) -> PyResult<PyString> {
        Ok(PyString::new(py, &self.pack_path(py).to_string_lossy()))
    }

    def get(&self, name: &PyBytes, node: &PyBytes) -> PyResult<PyBytes> {
        <DataStorePyExt>::get(self.store(py).as_ref(), py, name, node)
    }

    def getdeltachain(&self, name: &PyBytes, node: &PyBytes) -> PyResult<PyList> {
        <DataStorePyExt>::get_delta_chain(self.store(py).as_ref(), py, name, node)
    }

    def getmeta(&self, name: &PyBytes, node: &PyBytes) -> PyResult<PyDict> {
        <DataStorePyExt>::get_meta(self.store(py).as_ref(), py, name, node)
    }

    def getmissing(&self, keys: &PyList) -> PyResult<PyList> {
        <DataStorePyExt>::get_missing(self.store(py).as_ref(), py, keys)
    }
});

trait DataStorePyExt {
    fn get(&self, py: Python, name: &PyBytes, node: &PyBytes) -> PyResult<PyBytes>;
    fn get_delta_chain(&self, py: Python, name: &PyBytes, node: &PyBytes) -> PyResult<PyList>;
    fn get_meta(&self, py: Python, name: &PyBytes, node: &PyBytes) -> PyResult<PyDict>;
    fn get_missing(&self, py: Python, keys: &PyList) -> PyResult<PyList>;
}

impl<T: DataStore> DataStorePyExt for T {
    fn get(&self, py: Python, name: &PyBytes, node: &PyBytes) -> PyResult<PyBytes> {
        let key = to_key(py, name, node);
        let result = <DataStore>::get(self, &key).map_err(|e| to_pyerr(py, &e))?;

        Ok(PyBytes::new(py, &result[..]))
    }

    fn get_delta_chain(&self, py: Python, name: &PyBytes, node: &PyBytes) -> PyResult<PyList> {
        let key = to_key(py, name, node);
        let deltachain = self.get_delta_chain(&key).map_err(|e| to_pyerr(py, &e))?;
        let pychain = deltachain
            .iter()
            .map(|d| from_delta_to_tuple(py, &d))
            .collect::<Vec<PyObject>>();
        Ok(PyList::new(py, &pychain[..]))
    }

    fn get_meta(&self, py: Python, name: &PyBytes, node: &PyBytes) -> PyResult<PyDict> {
        let key = to_key(py, name, node);
        let metadata = self.get_meta(&key).map_err(|e| to_pyerr(py, &e))?;
        let metadict = PyDict::new(py);
        if let Some(size) = metadata.size {
            metadict.set_item(py, "s", size)?;
        }
        if let Some(flags) = metadata.flags {
            metadict.set_item(py, "f", flags)?;
        }

        Ok(metadict)
    }

    fn get_missing(&self, py: Python, keys: &PyList) -> PyResult<PyList> {
        // Copy the PyObjects into a vector so we can get a reference iterator.
        // This lets us get a Vector of Keys without copying the strings.
        let keys = keys.iter(py)
            .map(|k| from_tuple_to_key(py, &k))
            .collect::<Result<Vec<Key>, PyErr>>()?;
        let missing = self.get_missing(&keys[..]).map_err(|e| to_pyerr(py, &e))?;

        let results = PyList::new(py, &[]);
        for key in missing {
            let key_tuple = from_key_to_tuple(py, &key);
            results.insert_item(py, results.len(py), key_tuple.into_object());
        }

        Ok(results)
    }
}
