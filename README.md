# OneDrive Slideshow

A simple OneDrive Slideshow designed to be run on Windows or Linux.

## Installation

For Windows, the easiest way to install it is to use the [appinstaller file](https://dpaoliello.github.io/onedrive-slideshow/installer/onedrive-slideshow.appinstaller).

### Setup

The app will authenticate to OneDrive via the "Device Flow": it will provide a URL (that you will need to open on another device where you can log in to OneDrive) and a code to enter.

You will need to create a file called `slideshow.txt` in the root of your OneDrive that configures the slideshow. This is a JSON file with the basic format of:

```json
{
    "directories":
    [
        "Pictures"
    ],
    "interval": 15
}
```

* `directories` is the list of directories to search recursively for images.
* `interval` is the approximate number of seconds between each image.

You can also download this file from <https://dpaoliello.github.io/onedrive-slideshow/examples/simple/slideshow.txt>

## Contributing

### Building

OneDrive Slideshow is built in Rust, so building it requires the [Rust toolchain](https://rustup.rs) and then running:

```bash
cargo build --release
```
