pub mod entity {
    use rustc_hash::FxHashMap;
    use std::sync::RwLock;

    pub trait EntityMappingPersistor {
        fn get_entity(&self, hash: u64) -> Option<String>;
        fn put_data(&self, hash: u64, entity: String);
        fn contains(&self, hash: u64) -> bool;
    }

    #[derive(Debug, Default)]
    pub struct InMemoryEntityMappingPersistor {
        entity_mappings: RwLock<FxHashMap<u64, String>>,
    }

    impl EntityMappingPersistor for InMemoryEntityMappingPersistor {
        fn get_entity(&self, hash: u64) -> Option<String> {
            let entity_mappings_read = self.entity_mappings.read().unwrap();
            entity_mappings_read.get(&hash).map(|s| s.to_string())
        }

        fn put_data(&self, hash: u64, entity: String) {
            let mut entity_mappings_write = self.entity_mappings.write().unwrap();
            entity_mappings_write.insert(hash, entity);
        }

        fn contains(&self, hash: u64) -> bool {
            let entity_mappings_read = self.entity_mappings.read().unwrap();
            entity_mappings_read.contains_key(&hash)
        }
    }
}

pub mod embedding {
    use crate::io::S3File;
    use crate::persistence::embedding::memmap::OwnedMmapArrayViewMut;

    use ndarray::{s, Array};
    use ndarray_npy::write_zeroed_npy;
    use std::fs::File;
    use std::io;
    use std::io::{BufWriter, Error, ErrorKind, Write};

    use arrow2::{
        array::{Array as ArrowArray, Float32Array, UInt32Array, Utf8Array},
        chunk::Chunk,
        datatypes::{DataType, Field, Schema},
        error::Result as ArrowResult,
        io::parquet::write::{
            transverse, CompressionOptions, Encoding, FileWriter, RowGroupIterator, Version,
            WriteOptions,
        },
    };
    use chrono::prelude::*;

    pub trait EmbeddingPersistor {
        fn put_metadata(&mut self, entity_count: u32, dimension: u16) -> Result<(), io::Error>;

        fn put_data(
            &mut self,
            entity: &str,
            occur_count: u32,
            vector: Vec<f32>,
        ) -> Result<(), io::Error>;

        fn put_data_chunk(
            &mut self,
            chunk: (Vec<String>, Vec<u32>, Vec<Vec<f32>>),
        ) -> Result<(), io::Error>;

        fn finish(&mut self) -> Result<(), io::Error>;
    }

    pub struct TextFileVectorPersistor {
        buf_writer: BufWriter<File>,
        produce_entity_occurrence_count: bool,
    }

    impl TextFileVectorPersistor {
        pub fn new(filename: String, produce_entity_occurrence_count: bool) -> Self {
            let msg = format!("Unable to create file: {}", filename);
            let file = File::create(filename).expect(&msg);
            TextFileVectorPersistor {
                buf_writer: BufWriter::new(file),
                produce_entity_occurrence_count,
            }
        }
    }

    impl EmbeddingPersistor for TextFileVectorPersistor {
        fn put_metadata(&mut self, entity_count: u32, dimension: u16) -> Result<(), io::Error> {
            write!(&mut self.buf_writer, "{} {}", entity_count, dimension)?;
            Ok(())
        }

        fn put_data(
            &mut self,
            entity: &str,
            occur_count: u32,
            vector: Vec<f32>,
        ) -> Result<(), io::Error> {
            self.buf_writer.write_all(b"\n")?;
            self.buf_writer.write_all(entity.as_bytes())?;

            if self.produce_entity_occurrence_count {
                write!(&mut self.buf_writer, " {}", occur_count)?;
            }

            for &v in &vector {
                self.buf_writer.write_all(b" ")?;
                let mut buf = ryu::Buffer::new(); // cheap op
                self.buf_writer.write_all(buf.format_finite(v).as_bytes())?;
            }

            Ok(())
        }

        fn put_data_chunk(
            &mut self,
            chunk: (Vec<String>, Vec<u32>, Vec<Vec<f32>>),
        ) -> Result<(), io::Error> {
            let entities = chunk.0;
            let occur_counts = chunk.1;
            let vectors = &chunk.2;

            for i in 0..entities.len() {
                let entity = &entities[i];
                let occur_count = &occur_counts[i];
                let mut vector: Vec<f32> = Vec::new();

                vectors.into_iter().for_each(|x| vector.push(x[i]));
                self.put_data(entity.as_str(), *occur_count, vector)
                    .unwrap();
            }

            Ok(())
        }

        fn finish(&mut self) -> Result<(), io::Error> {
            self.buf_writer.write_all(b"\n")?;
            Ok(())
        }
    }

    pub struct ParquetVectorPersistor {
        schema: Schema,
        options: WriteOptions,
        encodings: Vec<Vec<Encoding>>,
        writer: FileWriter<Box<dyn Write>>,
        timestamp: String,
    }

    impl ParquetVectorPersistor {
        pub fn new(
            filename: String,
            dimension: u16,
        ) -> Self {
            let mut fields: Vec<Field> = vec![
                Field::new("entity", DataType::Utf8, false),
                Field::new("occur_count", DataType::UInt32, false),
                Field::new("datetime", DataType::Utf8, false),
                //Field::new("datetime", DataType::Timestamp(TimeUnit::Second, None), false),
            ];
            (0..dimension).into_iter().for_each(|x| {
                fields.push(Field::new(
                    format!("f{}", x).as_str(),
                    DataType::Float32,
                    false,
                ))
            });

            let schema = Schema::from(fields);

            let options = WriteOptions {
                write_statistics: false,
                compression: CompressionOptions::Snappy,
                version: Version::V2,
            };

            let encodings = schema
                .fields
                .iter()
                .map(|f| transverse(&f.data_type, |_| Encoding::Plain))
                .collect();

            // Create a new empty file
            let now = Utc::now();
            let f = now.format("%Y%m%dT%H%M%S").to_string();
            let file_name = filename.replace(".out", &format!("_{}.parquet", f));
            let file: Box<dyn Write> = if file_name.starts_with("s3://") {
                Box::new(S3File::create(file_name))
            } else {
                Box::new(File::create(file_name).unwrap())
            };

            let writer = FileWriter::try_new(file, schema.clone(), options.clone()).unwrap();

            let utc: String = now.format("%F %X").to_string();

            ParquetVectorPersistor {
                schema,
                options,
                encodings,
                writer,
                timestamp: utc,
            }
        }

        fn write_chunks(&mut self, chunk: Chunk<Box<dyn ArrowArray>>) -> ArrowResult<()> {
            let iter = vec![Ok(chunk)];

            let row_groups = RowGroupIterator::try_new(
                iter.into_iter(),
                &self.schema,
                self.options,
                self.encodings.clone(),
            )?;

            for group in row_groups {
                self.writer.write(group?)?;
            }

            Ok(())
        }
    }

    impl EmbeddingPersistor for ParquetVectorPersistor {
        fn put_metadata(&mut self, _entity_count: u32, _dimension: u16) -> Result<(), io::Error> {
            Ok(())
        }

        fn put_data(
            &mut self,
            _entity: &str,
            _occur_count: u32,
            _vector: Vec<f32>,
        ) -> Result<(), io::Error> {
            Ok(())
        }

        fn put_data_chunk(
            &mut self,
            chunk: (Vec<String>, Vec<u32>, Vec<Vec<f32>>),
        ) -> Result<(), io::Error> {
            let entities: Vec<Option<String>> = chunk.0.into_iter().map(|x| Some(x)).collect();
            let occur_counts: Vec<Option<u32>> = chunk.1.into_iter().map(|x| Some(x)).collect();

            let timestamps: Vec<Option<String>> = (0..entities.len())
                .into_iter()
                .map(|_x| Some(self.timestamp.clone()))
                .collect();

            let mut chunk_array = vec![
                Utf8Array::<i32>::from(entities).to_boxed(),
                UInt32Array::from(occur_counts).to_boxed(),
                Utf8Array::<i32>::from(timestamps).to_boxed(),
            ];

            chunk.2.into_iter().for_each(|x| {
                chunk_array.push(
                    Float32Array::from(
                        x.into_iter().map(|e| Some(e)).collect::<Vec<Option<f32>>>(),
                    )
                    .to_boxed(),
                )
            });

            let chunk = Chunk::new(chunk_array);
            self.write_chunks(chunk).unwrap();

            Ok(())
        }

        fn finish(&mut self) -> Result<(), io::Error> {
            let _size = self.writer.end(None).unwrap();
            Ok(())
        }
    }

    mod memmap {
        use memmap::MmapMut;
        use ndarray::ArrayViewMut2;
        use std::fs::OpenOptions;
        use std::io;
        use std::io::{Error, ErrorKind};
        use std::ptr::drop_in_place;

        pub struct OwnedMmapArrayViewMut {
            mmap_ptr: *mut MmapMut,
            mmap_data: Option<ndarray::ArrayViewMut2<'static, f32>>,
        }

        impl OwnedMmapArrayViewMut {
            pub fn new(filename: &str) -> Result<Self, io::Error> {
                use ndarray_npy::ViewMutNpyExt;

                let file = OpenOptions::new().read(true).write(true).open(filename)?;
                let mmap = unsafe { MmapMut::map_mut(&file)? };
                let mmap = Box::new(mmap);
                let mmap = Box::leak(mmap);
                let mmap_ptr: *mut MmapMut = mmap as *mut _;

                let mmap_data = ArrayViewMut2::<'static, f32>::view_mut_npy(mmap)
                    .map_err(|_| Error::new(ErrorKind::Other, "Mmap view error"))?;

                Ok(Self {
                    mmap_ptr,
                    mmap_data: Some(mmap_data),
                })
            }

            pub fn data_view<'a>(&'a mut self) -> &'a mut ArrayViewMut2<'a, f32> {
                let view = self
                    .mmap_data
                    .as_mut()
                    .expect("Should be always defined. None only used in Drop");

                // SAFETY: shortening lifetime from 'static to 'a is safe because underlying buffer won't be dropped until view is borrowed
                unsafe {
                    core::mem::transmute::<
                        &mut ArrayViewMut2<'static, f32>,
                        &mut ArrayViewMut2<'a, f32>,
                    >(view)
                }
            }
        }

        impl Drop for OwnedMmapArrayViewMut {
            fn drop(&mut self) {
                // Unwind references with reverse order.
                // First remove view that points to mmap_ptr
                self.mmap_data = None;
                // And now drop mmap_ptr
                // SAFETY: safe because pointer leaked in constructor.
                unsafe { drop_in_place(self.mmap_ptr) }
            }
        }
    }

    pub struct NpyPersistor {
        entities: Vec<String>,
        occurences: Vec<u32>,
        array_file_name: String,
        array_file: File,
        array_write_context: Option<OwnedMmapArrayViewMut>,
        occurences_buf: Option<BufWriter<File>>,
        entities_buf: BufWriter<File>,
    }

    impl NpyPersistor {
        pub fn new(filename: String, produce_entity_occurrence_count: bool) -> Self {
            let entities_filename = format!("{}.entities", &filename);
            let entities_buf = BufWriter::new(
                File::create(&entities_filename)
                    .unwrap_or_else(|_| panic!("Unable to create file: {}", &entities_filename)),
            );

            let occurences_filename = format!("{}.occurences", &filename);
            let occurences_buf = if produce_entity_occurrence_count {
                Some(BufWriter::new(
                    File::create(&occurences_filename).unwrap_or_else(|_| {
                        panic!("Unable to create file: {}", &occurences_filename)
                    }),
                ))
            } else {
                None
            };

            let array_file_name = format!("{}.npy", &filename);
            let array_file = File::create(&array_file_name)
                .unwrap_or_else(|_| panic!("Unable to create file: {}", &array_file_name));

            Self {
                entities: vec![],
                occurences: vec![],
                array_file_name,
                array_file,
                array_write_context: None,
                occurences_buf,
                entities_buf,
            }
        }
    }

    impl EmbeddingPersistor for NpyPersistor {
        fn put_metadata(&mut self, entity_count: u32, dimension: u16) -> Result<(), io::Error> {
            write_zeroed_npy::<f32, _>(
                &self.array_file,
                [entity_count as usize, dimension as usize],
            )
            .map_err(|_| Error::new(ErrorKind::Other, "Write zeroed npy error"))?;
            self.array_write_context = Some(OwnedMmapArrayViewMut::new(&self.array_file_name)?);
            Ok(())
        }

        fn put_data(
            &mut self,
            entity: &str,
            occur_count: u32,
            vector: Vec<f32>,
        ) -> Result<(), io::Error> {
            let array = &mut self
                .array_write_context
                .as_mut()
                .expect("Should be defined. Was put_metadata not called?")
                .data_view();

            array
                .slice_mut(s![self.entities.len(), ..])
                .assign(&Array::from(vector));
            self.entities.push(entity.to_owned());
            self.occurences.push(occur_count);
            Ok(())
        }

        fn put_data_chunk(
            &mut self,
            chunk: (Vec<String>, Vec<u32>, Vec<Vec<f32>>),
        ) -> Result<(), io::Error> {
            let entities = chunk.0;
            let occur_counts = chunk.1;
            let vectors = &chunk.2;

            for i in 0..entities.len() {
                let entity = &entities[i];
                let occur_count = &occur_counts[i];
                let mut vector: Vec<f32> = Vec::new();

                vectors.into_iter().for_each(|x| vector.push(x[i]));
                self.put_data(entity.as_str(), *occur_count, vector)
                    .unwrap();
            }

            Ok(())
        }

        fn finish(&mut self) -> Result<(), io::Error> {
            use ndarray_npy::WriteNpyExt;

            serde_json::to_writer_pretty(&mut self.entities_buf, &self.entities)?;

            if let Some(occurences_buf) = self.occurences_buf.as_mut() {
                let occur = ndarray::ArrayView1::from(&self.occurences);
                occur.write_npy(occurences_buf).map_err(|e| {
                    Error::new(
                        ErrorKind::Other,
                        format!("Could not save occurences: {}", e),
                    )
                })?;
            }

            Ok(())
        }
    }
}
