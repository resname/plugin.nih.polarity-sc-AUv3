cmake_minimum_required(VERSION 3.21)
project(polarity_sc_auv3 LANGUAGES CXX OBJC OBJCXX)

set(CLAP_WRAPPER_DOWNLOAD_DEPENDENCIES TRUE CACHE BOOL "Download clap-headers for the wrapper")
set(POLARITY_CLAP_LOCATION "" CACHE PATH "Absolute path to the built Resname-Spectral-Compressor.clap bundle")

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
        message(FATAL_ERROR "clap-wrapper: POLARITY_CLAP_LOCATION must point to the built Resname-Spectral-Compressor.clap bundle")
    endif()
    add_executable(polarity_sc_dark_as_auv3)
    # Embed the CLAP into the appex's Contents/PlugIns/. pinned clap-wrapper does
    # this too (macos_include_clap_in_bundle), but its destination is relative to
    # the LIBRARY_OUTPUT_DIRECTORY property, which under the Xcode generator does
    # not resolve inside the built .appex — the CI appex shipped without a
    # PlugIns dir and the AU then failed to load ("Cannot load CLAP plugin").
    # A PRE_BUILD copy with an absolute bundle dir also stays ahead of the
    # wrapper's POST_BUILD ad-hoc codesign, so the CLAP is covered by the
    # signature. `codesign --deep` is not needed at runtime; the nested un-signed
    # .clap Mach-O is accepted as a plugin payload of a signed appex.
    add_custom_command(TARGET polarity_sc_dark_as_auv3 PRE_BUILD
            COMMAND ${CMAKE_COMMAND} -E make_directory
                "$<TARGET_BUNDLE_DIR:polarity_sc_dark_as_auv3>/Contents/PlugIns"
            COMMAND ${CMAKE_COMMAND} -E copy_directory
                "${POLARITY_CLAP_LOCATION}"
                "$<TARGET_BUNDLE_DIR:polarity_sc_dark_as_auv3>/Contents/PlugIns/Resname-Spectral-Compressor.clap"
            COMMENT "Embedding Resname-Spectral-Compressor.clap into the AUv3 appex"
    )
    target_add_auv3_wrapper(
            TARGET polarity_sc_dark_as_auv3
            OUTPUT_NAME "Resname-Spectral-Compressor"
            BUNDLE_IDENTIFIER "de.resname.resname-spectral-compressor.auv3"
            BUNDLE_VERSION "${POLARITY_PLUGIN_VERSION}"
            INSTRUMENT_TYPE "aufx"
            MANUFACTURER_NAME "resname"
            MANUFACTURER_CODE "Rsnm"
            SUBTYPE_CODE "Rscm"
            # Fixed plugin GUI size (ViziaState in the Rust plugin). The
            # build helper writes it into the appex Info.plist and the view
            # controller seeds preferredContentSize from it — the viewbridge
            # runs loadView before any audio unit exists, so a build-time
            # constant is the only source hosts capturing preferredContentSize
            # once at view capture can use. Update when the plugin GUI size
            # changes.
            PREFERRED_GUI_SIZE "1360x690"
            MACOS_EMBEDDED_CLAP_LOCATION "${POLARITY_CLAP_LOCATION}"
            # Sandboxed appex + outbound access for the GUI's update check:
            # without com.apple.security.network.client every connection dies
            # in the sandbox and the update-available strip never shows. See
            # resname.entitlements.
            ENTITLEMENTS "${CMAKE_CURRENT_SOURCE_DIR}/resname.entitlements"
    )
    add_executable(polarity_sc_dark_as_auv3_standalone)
    target_add_auv3_standalone_wrapper(
            TARGET polarity_sc_dark_as_auv3_standalone
            OUTPUT_NAME "Resname Spectral Compressor AUv3"
            BUNDLE_IDENTIFIER "de.resname.resname-spectral-compressor.auv3standalone"
            BUNDLE_VERSION "${POLARITY_PLUGIN_VERSION}"
            AUV3_TARGET polarity_sc_dark_as_auv3
            AU_TYPE "aufx"
            AU_SUBTYPE "Rscm"
            AU_MANUFACTURER "Rsnm"
    )

    # Swap the wrapper's AUv3HostAppDelegate.mm for our local patched copy
    # (auv3/standalone_host/): the upstream version connects the hardware
    # input with the raw hardware format, so a mono built-in microphone
    # throws kAudioUnitErr_FormatNotSupported (-10868) against the appex's
    # stereo input bus ("Failed to set up audio engine"). Same fix is being
    # proposed upstream; remove this override when the submodule pin picks
    # it up.
    get_target_property(_ausa_sources polarity_sc_dark_as_auv3_standalone SOURCES)
    list(REMOVE_ITEM _ausa_sources
            "${CLAP_WRAPPER_CMAKE_CURRENT_SOURCE_DIR}/src/detail/standalone/macos/auv3/AUv3HostAppDelegate.mm")
    list(APPEND _ausa_sources "${CMAKE_CURRENT_SOURCE_DIR}/standalone_host/AUv3HostAppDelegate.mm")
    set_target_properties(polarity_sc_dark_as_auv3_standalone PROPERTIES SOURCES "${_ausa_sources}")
endif()
