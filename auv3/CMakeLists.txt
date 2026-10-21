cmake_minimum_required(VERSION 3.21)
project(polarity_sc_auv3 LANGUAGES CXX OBJC OBJCXX)

set(CLAP_WRAPPER_DOWNLOAD_DEPENDENCIES TRUE CACHE BOOL "Download clap-headers for the wrapper")
set(POLARITY_CLAP_LOCATION "" CACHE PATH "Absolute path to the built Polarity-SC-Dark.clap bundle")

add_subdirectory(clap-wrapper EXCLUDE_FROM_ALL)

# Pull the plugin's version from the Rust manifest so the bundle versions
# track it instead of a hardcoded placeholder.
file(STRINGS "${CMAKE_CURRENT_SOURCE_DIR}/../plugin/Cargo.toml" _pcv REGEX "^version *= *\"[^\"]+\"")
string(REGEX REPLACE "^version *= *\"([^\"]+)\".*" "\\1" POLARITY_PLUGIN_VERSION "${_pcv}")
if ("${POLARITY_PLUGIN_VERSION}" STREQUAL "")
    set(POLARITY_PLUGIN_VERSION "1.0")
endif()
if (APPLE)
    if ("${POLARITY_CLAP_LOCATION}" STREQUAL "")
        message(FATAL_ERROR "clap-wrapper: POLARITY_CLAP_LOCATION must point to the built Polarity-SC-Dark.clap bundle")
    endif()
    add_executable(polarity_sc_dark_as_auv3)
    target_add_auv3_wrapper(
            TARGET polarity_sc_dark_as_auv3
            OUTPUT_NAME "Polarity-SC-Dark"
            BUNDLE_IDENTIFIER "de.polarity-music.polarity-sc-dark.auv3"
            BUNDLE_VERSION "${POLARITY_PLUGIN_VERSION}"
            INSTRUMENT_TYPE "aufx"
            MANUFACTURER_NAME "polarity-music"
            MANUFACTURER_CODE "Plsc"
            SUBTYPE_CODE "PscD"
            MACOS_EMBEDDED_CLAP_LOCATION "${POLARITY_CLAP_LOCATION}"
    )
    add_executable(polarity_sc_dark_as_auv3_standalone)
    target_add_auv3_standalone_wrapper(
            TARGET polarity_sc_dark_as_auv3_standalone
            OUTPUT_NAME "Polarity-SC-Dark AUv3"
            BUNDLE_IDENTIFIER "de.polarity-music.polarity-sc-dark.auv3standalone"
            BUNDLE_VERSION "${POLARITY_PLUGIN_VERSION}"
            AUV3_TARGET polarity_sc_dark_as_auv3
            AU_TYPE "aufx"
            AU_SUBTYPE "PscD"
            AU_MANUFACTURER "Plsc"
    )
endif()
