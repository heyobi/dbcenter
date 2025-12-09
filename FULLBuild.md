cargo build --release -p dbcenter-python
C:\ProgramData\miniconda3\Scripts\conda.exe run -p c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\.conda pip install maturin
C:\ProgramData\miniconda3\Scripts\conda.exe run -p c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\.conda maturin develop --release
cd c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\dbcenter-python; C:\ProgramData\miniconda3\Scripts\conda.exe run -p c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\.conda maturin build --release


-- watcher exe yi whl ye gömdük
cd c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\dbcenter-python; C:\ProgramData\miniconda3\Scripts\conda.exe run -p c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\.conda maturin build --release


cd c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\dbcenter-python; C:\ProgramData\miniconda3\Scripts\conda.exe run -p c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\.conda maturin build --release


cargo fmt -p dbcenter-core -p dbcenter-python -p dbcenter-ffi

içerik görme
cd c:\Users\Dell\Downloads\dbcenter\rust-dbcenter; C:\ProgramData\miniconda3\Scripts\conda.exe run -p c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\.conda python -m zipfile -l target\wheels\dbcenter-0.1.0-cp312-cp312-win_amd64.whl



cd c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\dbcenter-python; C:\ProgramData\miniconda3\Scripts\conda.exe run -p c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\.conda maturin build --release



BU ikisi yeterli
cd c:\Users\Dell\Downloads\dbcenter\rust-dbcenter; cargo fmt

cd c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\dbcenter-python; C:\ProgramData\miniconda3\Scripts\conda.exe run -p c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\.conda maturin build --release







artık bu kod yeterli

cd 'c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\dbcenter-python'                           
PS C:\Users\Dell\Downloads\dbcenter\rust-dbcenter\dbcenter-python> C:\ProgramData\miniconda3\Scripts\conda.exe run -p c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\.conda maturin build --release; C:\ProgramData\miniconda3\Scripts\conda.exe run -n dbcenter312 pip install --force-reinstall c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\target\wheels\dbcenter-0.1.0-cp312-cp312-win_amd64.whl; taskkill /F /IM postgres.exe; C:\ProgramData\miniconda3\Scripts\conda.exe run -n dbcenter312 python performance_test_vector.py




Bu komut linuxa derler
C:\ProgramData\miniconda3\Scripts\conda.exe run -p c:\Users\Dell\Downloads\dbcenter\rust-dbcenter\.conda cmd /c "set PATH=C:\Users\Dell\Downloads\dbcenter\rust-dbcenter\.conda\Lib\site-packages\ziglang;C:\Users\Dell\Downloads\dbcenter\rust-dbcenter\.conda\Scripts;%PATH% && maturin build --release --target x86_64-unknown-linux-gnu --zig -v"