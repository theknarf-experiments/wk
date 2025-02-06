# WK

`wk` the workspace tool

## Setup

First install SDL2:

```
brew install sdl2
```

Then setup the env variable:

```
export LIBRARY_PATH="$LIBRARY_PATH:$(brew --prefix)/lib"
```

and then run:

```
cargo run -- example1
```
