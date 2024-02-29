## Easy Setup
1. Download dataset & pretrined models
2. Place the dataset in VQA/data/VQA_Workflow_Dataset
3. Place the pretrained model in VQA/python/trained_models/variants/

## How to use Jellybean?
Jellybean consists of three components, Profiler, Optimizer(Part1: Model, Part2: Worker), and Processor.

## Start to use (commands)

### Profiling

- profile message size
    -  (1'30) python3 python/models_profiler/compute_message_size.py --dataset data/VQA_Workflow_Datasets --output profile_result

- Profile model latency
    
    - (4'26) python3 python/models_profiler/profile_asr.py --dataset data/VQA_Workflow_Datasets --output profile_result --device cpu 

    - (1'42) python python/models_profiler/profile_image_model.py --output profile_result --device cpu  
    - () python python/models_profiler/profile_vqa.py --dataset data/VQA_Workflow_Datasets --output profile_result --device cpu


### Optimization
python optimizer/main.py --inputs VQA/examples/optimizer_inputs/ --config VQA/examples/optimizer_inputs/config.yaml