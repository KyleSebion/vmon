idf_component_register(
    SRCS "rwdt_shim.c"
    INCLUDE_DIRS "include"
    REQUIRES esp_hw_support hal
)