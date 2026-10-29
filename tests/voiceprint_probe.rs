#[test]
fn inspect_model() {
 let mut s=ort::session::Session::builder().unwrap().with_intra_threads(2).unwrap().with_inter_threads(1).unwrap().commit_from_file("/home/snippet/.snippet/models/wespeaker-resnet34-lm-v1.onnx").unwrap();
 eprintln!("inputs {:?} outputs {:?}",s.inputs.iter().map(|x| (x.name.clone(),x.input_type.clone())).collect::<Vec<_>>(),s.outputs.iter().map(|x|(x.name.clone(),x.output_type.clone())).collect::<Vec<_>>());
}
