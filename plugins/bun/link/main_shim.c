// wasi-libc's __main_void calls __main_argc_argv (the C main-rename target).
// Bun's Rust entry is exported as plain `main`, so bridge the two here.
extern int main(int argc, char** argv);
int __main_argc_argv(int argc, char** argv) { return main(argc, argv); }
