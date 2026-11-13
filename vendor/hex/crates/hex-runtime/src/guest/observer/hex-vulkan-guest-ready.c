/* hex-vulkan-guest-ready: the paid Linux guest Program whose frame
 * guest/observer/driver.rs decodes (AC-T7.4.1). Native grant/scope/ACK owns host
 * identity; these Vulkan observations do not. Build it only with the target's
 * reviewed loader/ICD closure; the observer configuration pins every artifact by
 * size and SHA-256. No implicit ICD or PATH fallback.
 */
#define _POSIX_C_SOURCE 200809L
#include <vulkan/vulkan.h>
#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

struct limits {
    uint32_t devices, queues;
    size_t allocation, output;
    uint64_t wait_ns;
    const char *icd;
};

struct output {
    size_t left;
    uint64_t deadline;
};

struct observation {
    VkPhysicalDevice physical;
    VkPhysicalDeviceProperties2 properties;
    VkPhysicalDeviceIDProperties ids;
    VkPhysicalDeviceDriverProperties driver;
    VkPhysicalDeviceMemoryProperties memory;
    uint32_t queues, compute, queue_count;
    VkQueueFlags queue_flags;
    bool accepted;
};

/* Nanoseconds/second is the POSIX timespec unit, not a learned allowance. */
#define NS_PER_SECOND UINT64_C(1000000000)

static bool now_ns(uint64_t *value)
{
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 || now.tv_sec < 0 ||
        now.tv_nsec < 0 || (uint64_t)now.tv_nsec >= NS_PER_SECOND ||
        (uint64_t)now.tv_sec > (UINT64_MAX - (uint64_t)now.tv_nsec) / NS_PER_SECOND)
        return false;
    *value = (uint64_t)now.tv_sec * NS_PER_SECOND + (uint64_t)now.tv_nsec;
    return true;
}

static bool remaining(uint64_t deadline, uint64_t *left)
{
    uint64_t now;
    if (!now_ns(&now) || now >= deadline)
        return false;
    *left = deadline - now;
    return true;
}

static bool write_all(int fd, const char *bytes, size_t count)
{
    while (count != 0) {
        ssize_t wrote = write(fd, bytes, count);
        if (wrote < 0 && errno == EINTR)
            continue;
        if (wrote <= 0 || (size_t)wrote > count)
            return false;
        bytes += (size_t)wrote;
        count -= (size_t)wrote;
    }
    return true;
}

static size_t decimal(uint64_t value, char *buffer)
{
    /* Binary digit width is an upper bound for decimal, including zero. */
    char reverse[sizeof(uint64_t) * CHAR_BIT];
    size_t count = 0;
    do {
        reverse[count++] = (char)('0' + value % 10);
        value /= 10;
    } while (value != 0);
    for (size_t i = 0; i < count; ++i)
        buffer[i] = reverse[count - i - 1];
    return count;
}

static int refused(const char *stage, int64_t result)
{
    char digits[sizeof(uint64_t) * CHAR_BIT];
    const char *prefix = "hex-vulkan-guest-ready: ";
    bool ignored = write_all(STDERR_FILENO, prefix, strlen(prefix));
    ignored = write_all(STDERR_FILENO, stage, strlen(stage)) && ignored;
    ignored = write_all(STDERR_FILENO, " ", 1) && ignored;
    if (result < 0)
        ignored = write_all(STDERR_FILENO, "-", 1) && ignored;
    uint64_t magnitude = result < 0 ? (uint64_t)(-(result + 1)) + 1 : (uint64_t)result;
    size_t count = decimal(magnitude, digits);
    ignored = write_all(STDERR_FILENO, digits, count) && ignored;
    ignored = write_all(STDERR_FILENO, "\n", 1) && ignored;
    (void)ignored;
    return EXIT_FAILURE;
}

static bool emit(struct output *out, const char *bytes, size_t count)
{
    uint64_t left;
    if (count > out->left || !remaining(out->deadline, &left) ||
        !write_all(STDOUT_FILENO, bytes, count))
        return false;
    out->left -= count;
    return true;
}

static bool literal(struct output *out, const char *word)
{
    return emit(out, word, strlen(word));
}

static bool scalar(struct output *out, uint64_t value)
{
    char digits[sizeof(uint64_t) * CHAR_BIT];
    size_t count = decimal(value, digits);
    return literal(out, " ") && emit(out, digits, count);
}

static bool hex(struct output *out, const void *value, size_t count)
{
    const unsigned char *bytes = value;
    const char alphabet[] = "0123456789abcdef";
    if (!literal(out, " "))
        return false;
    if (count == 0)
        return literal(out, "-");
    for (size_t i = 0; i < count; ++i) {
        char pair[] = {alphabet[bytes[i] >> 4], alphabet[bytes[i] & 15]};
        if (!emit(out, pair, sizeof(pair)))
            return false;
    }
    return true;
}

static bool native_text(struct output *out, const char *text, size_t bound)
{
    const char *end = memchr(text, '\0', bound);
    return end != NULL && hex(out, text, (size_t)(end - text));
}

static bool positive(const char *text, uint64_t *number)
{
    if (text == NULL || *text < '1' || *text > '9')
        return false;
    uint64_t value = 0;
    for (const char *p = text; *p != '\0'; ++p) {
        if (*p < '0' || *p > '9' || value > (UINT64_MAX - (uint64_t)(*p - '0')) / 10)
            return false;
        value = value * 10 + (uint64_t)(*p - '0');
    }
    *number = value;
    return true;
}

static bool arguments(int argc, char **argv, struct limits *limit)
{
    /* Exactly six required flag/value pairs, each accepted once. */
    if (argc != 1 + 6 * 2)
        return false;
    unsigned seen = 0;
    for (int i = 1; i < argc; i += 2) {
        const char *flag = argv[i], *value = argv[i + 1];
        unsigned bit;
        uint64_t number = 0;
        if (strcmp(flag, "--icd") == 0) {
            bit = 1u << 0;
            if (*value != '/')
                return false;
            limit->icd = value;
        } else {
            if (!positive(value, &number))
                return false;
            if (strcmp(flag, "--expected-devices") == 0 && number <= UINT32_MAX) {
                bit = 1u << 1;
                limit->devices = (uint32_t)number;
            } else if (strcmp(flag, "--max-queue-families") == 0 && number <= UINT32_MAX) {
                bit = 1u << 2;
                limit->queues = (uint32_t)number;
            } else if (strcmp(flag, "--max-allocation-bytes") == 0 && number <= SIZE_MAX) {
                bit = 1u << 3;
                limit->allocation = (size_t)number;
            } else if (strcmp(flag, "--max-output-bytes") == 0 && number <= SIZE_MAX) {
                bit = 1u << 4;
                limit->output = (size_t)number;
            } else if (strcmp(flag, "--wait-ns") == 0 && number != UINT64_MAX) {
                bit = 1u << 5;
                limit->wait_ns = number;
            } else {
                return false;
            }
        }
        if ((seen & bit) != 0)
            return false;
        seen |= bit;
    }
    const char *icd = getenv("VK_DRIVER_FILES");
    const char *layers = getenv("VK_LOADER_LAYERS_DISABLE");
    return seen == (1u << 6) - 1 && icd != NULL && layers != NULL &&
        strcmp(icd, limit->icd) == 0 && strcmp(layers, "~all~") == 0;
}

static bool storage(size_t count, size_t unit, size_t *left, void **pointer)
{
    if (count == 0 || unit == 0 || count > SIZE_MAX / unit || count * unit > *left)
        return false;
    *pointer = calloc(count, unit);
    if (*pointer == NULL)
        return false;
    *left -= count * unit;
    return true;
}

static VkResult observe(struct observation *row, VkQueueFamilyProperties *queues,
                        uint32_t capacity)
{
    row->properties.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2;
    row->ids.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES;
    row->driver.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES;
    vkGetPhysicalDeviceProperties(row->physical, &row->properties.properties);
    if (row->properties.properties.apiVersion < VK_API_VERSION_1_2)
        return VK_ERROR_INCOMPATIBLE_DRIVER;
    row->properties.pNext = &row->ids;
    row->ids.pNext = &row->driver;
    vkGetPhysicalDeviceProperties2(row->physical, &row->properties);
    vkGetPhysicalDeviceMemoryProperties(row->physical, &row->memory);
    if (row->memory.memoryHeapCount == 0 || row->memory.memoryHeapCount > VK_MAX_MEMORY_HEAPS)
        return VK_ERROR_INITIALIZATION_FAILED;
    vkGetPhysicalDeviceQueueFamilyProperties(row->physical, &row->queues, NULL);
    if (row->queues == 0 || row->queues > capacity)
        return VK_ERROR_INITIALIZATION_FAILED;
    uint32_t written = row->queues;
    vkGetPhysicalDeviceQueueFamilyProperties(row->physical, &written, queues);
    uint32_t stable = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(row->physical, &stable, NULL);
    if (written != row->queues || stable != row->queues)
        return VK_ERROR_INITIALIZATION_FAILED;
    row->compute = row->queues; /* Missing queue is diagnostic, never usable. */
    for (uint32_t i = 0; i < written; ++i) {
        if (queues[i].queueCount != 0 && (queues[i].queueFlags & VK_QUEUE_COMPUTE_BIT) != 0) {
            row->compute = i;
            row->queue_count = queues[i].queueCount;
            row->queue_flags = queues[i].queueFlags;
            break;
        }
    }
    bool local = false;
    for (uint32_t i = 0; i < row->memory.memoryHeapCount; ++i)
        local |= (row->memory.memoryHeaps[i].flags & VK_MEMORY_HEAP_DEVICE_LOCAL_BIT) != 0 &&
            row->memory.memoryHeaps[i].size != 0;
    VkPhysicalDeviceType type = row->properties.properties.deviceType;
    const char venus[] = "venus";
    row->accepted = (type == VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU ||
        type == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU || type == VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU) &&
        row->driver.driverID == VK_DRIVER_ID_MESA_VENUS &&
        memcmp(row->driver.driverName, venus, sizeof(venus)) == 0 && local &&
        row->compute < row->queues;
    return VK_SUCCESS;
}

static bool emit_observation(struct output *out, const struct observation *row, uint32_t index)
{
    const VkPhysicalDeviceProperties *p = &row->properties.properties;
    const VkConformanceVersion *v = &row->driver.conformanceVersion;
    if (!literal(out, "device") || !scalar(out, index) || !scalar(out, p->apiVersion) ||
        !scalar(out, (uint32_t)row->driver.driverID) || !scalar(out, p->driverVersion) ||
        !scalar(out, p->vendorID) || !scalar(out, p->deviceID) || !scalar(out, (uint32_t)p->deviceType) ||
        !hex(out, row->ids.deviceUUID, VK_UUID_SIZE) ||
        !native_text(out, p->deviceName, sizeof(p->deviceName)) ||
        !native_text(out, row->driver.driverName, sizeof(row->driver.driverName)) ||
        !native_text(out, row->driver.driverInfo, sizeof(row->driver.driverInfo)) ||
        !scalar(out, row->queues) || !scalar(out, row->compute) || !scalar(out, row->queue_count) ||
        !scalar(out, row->queue_flags) || !scalar(out, row->memory.memoryHeapCount) ||
        !literal(out, "\ndriver") || !scalar(out, index) ||
        !hex(out, row->ids.driverUUID, VK_UUID_SIZE) || !scalar(out, v->major) ||
        !scalar(out, v->minor) || !scalar(out, v->subminor) || !scalar(out, v->patch) ||
        !literal(out, "\n"))
        return false;
    for (uint32_t i = 0; i < row->memory.memoryHeapCount; ++i)
        if (!literal(out, "heap") || !scalar(out, index) || !scalar(out, i) ||
            !scalar(out, row->memory.memoryHeaps[i].flags) ||
            !scalar(out, row->memory.memoryHeaps[i].size) || !literal(out, "\n"))
            return false;
    return true;
}

static VkResult work_ready(const struct observation *row, uint64_t deadline, bool *in_flight)
{
    uint64_t left;
    if (!remaining(deadline, &left))
        return VK_TIMEOUT;
    const float priority = 1.0f; /* Vulkan queuePriority's normalized upper endpoint. */
    VkDeviceQueueCreateInfo queue_info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = row->compute, .queueCount = 1, .pQueuePriorities = &priority,
    };
    VkDeviceCreateInfo create = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1, .pQueueCreateInfos = &queue_info,
    };
    VkDevice device = VK_NULL_HANDLE;
    VkResult result = vkCreateDevice(row->physical, &create, NULL, &device);
    if (result != VK_SUCCESS)
        return result;
    VkQueue queue = VK_NULL_HANDLE;
    vkGetDeviceQueue(device, row->compute, 0, &queue);
    if (queue == VK_NULL_HANDLE || !remaining(deadline, &left)) {
        vkDestroyDevice(device, NULL);
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VkFence fence = VK_NULL_HANDLE;
    result = vkCreateFence(device, &fence_info, NULL, &fence);
    if (result != VK_SUCCESS) {
        vkDestroyDevice(device, NULL);
        return result;
    }
    VkSubmitInfo submit = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO};
    /* No shader/command buffers: only actual queue submission and completion.
     * This is intentionally weaker than the independent compute oracle. */
    *in_flight = true;
    result = vkQueueSubmit(queue, 1, &submit, fence);
    if (result == VK_SUCCESS) {
        result = remaining(deadline, &left) ?
            vkWaitForFences(device, 1, &fence, VK_TRUE, left) : VK_TIMEOUT;
    }
    if (result != VK_SUCCESS)
        return result;
    *in_flight = false;
    vkDestroyFence(device, fence, NULL);
    vkDestroyDevice(device, NULL);
    return VK_SUCCESS;
}

int main(int argc, char **argv)
{
    struct limits limit = {0};
    if (!arguments(argc, argv, &limit))
        return refused("arguments", 0);
    uint64_t start;
    if (!now_ns(&start) || limit.wait_ns > UINT64_MAX - start)
        return refused("deadline", 0);
    struct output out = {.left = limit.output, .deadline = start + limit.wait_ns};
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_2,
    };
    VkInstanceCreateInfo create = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app,
    };
    VkInstance instance = VK_NULL_HANDLE;
    VkResult result = vkCreateInstance(&create, NULL, &instance);
    if (result != VK_SUCCESS)
        return refused("create-instance", result);
    VkPhysicalDevice *physical = NULL;
    struct observation *rows = NULL;
    VkQueueFamilyProperties *queues = NULL;
    const char *stage = "enumerate-count";
    /* What a refusal says after its stage: the Vulkan result, or at "queue-families" the device's
     * own count, which --max-queue-families must hold (the census's record of a bound run out). */
    int64_t said;
    uint32_t count = 0;
    result = vkEnumeratePhysicalDevices(instance, &count, NULL);
    if (result != VK_SUCCESS)
        goto fail;
    stage = "visible-count";
    if (!literal(&out, "hex-vulkan-guest-readiness 1") || !scalar(&out, count) ||
        !literal(&out, "\n") || count != limit.devices || count == 0) {
        result = VK_ERROR_INITIALIZATION_FAILED;
        goto fail;
    }
    stage = "allocation";
    size_t room = limit.allocation;
    void *allocation = NULL;
    if (!storage(count, sizeof(*physical), &room, &allocation))
        goto memory;
    physical = allocation;
    if (!storage(count, sizeof(*rows), &room, &allocation))
        goto memory;
    rows = allocation;
    if (!storage(limit.queues, sizeof(*queues), &room, &allocation))
        goto memory;
    queues = allocation;
    uint32_t written = count;
    stage = "enumerate-set";
    result = vkEnumeratePhysicalDevices(instance, &written, physical);
    if (result != VK_SUCCESS || written != count) {
        if (result == VK_SUCCESS)
            result = VK_INCOMPLETE;
        goto fail;
    }
    bool accepted = true;
    for (uint32_t i = 0; i < count; ++i) {
        uint64_t left;
        stage = "observe";
        if (!remaining(out.deadline, &left)) {
            result = VK_TIMEOUT;
            goto fail;
        }
        rows[i].physical = physical[i];
        result = observe(&rows[i], queues, limit.queues);
        if (result != VK_SUCCESS && rows[i].queues > limit.queues) {
            stage = "queue-families";
            said = rows[i].queues;
            goto refuse;
        }
        if (result != VK_SUCCESS)
            goto fail;
        stage = "bounded-output";
        if (!emit_observation(&out, &rows[i], i)) {
            result = VK_ERROR_INITIALIZATION_FAILED;
            goto fail;
        }
        accepted &= rows[i].accepted;
    }
    /* Report CPU/unknown/foreign rows, then refuse the entire set; never drop a
     * row to make a selected count or silently choose a different renderer. */
    stage = "non-venus-hardware-or-no-compute-heap";
    if (!accepted) {
        result = VK_ERROR_INCOMPATIBLE_DRIVER;
        goto fail;
    }
    for (uint32_t i = 0; i < count; ++i) {
        bool in_flight = false;
        result = work_ready(&rows[i], out.deadline, &in_flight);
        stage = "queue-fence";
        if (result != VK_SUCCESS) {
            if (in_flight) {
                /* Completion is uncertain. Do not destroy an in-use fence,
                 * call unbounded DeviceWaitIdle, or claim Vulkan cleanup. Exit
                 * this paid Process; the SAME outer grant remains retained. */
                int code = refused(stage, result);
                _Exit(code);
            }
            goto fail;
        }
    }
    free(queues);
    free(rows);
    free(physical);
    vkDestroyInstance(instance, NULL);
    if (!literal(&out, "ready") || !scalar(&out, count) || !literal(&out, "\n"))
        return refused("completion-output", 0);
    return EXIT_SUCCESS;
memory:
    result = VK_ERROR_OUT_OF_HOST_MEMORY;
fail:
    said = result;
refuse:
    free(queues);
    free(rows);
    free(physical);
    vkDestroyInstance(instance, NULL);
    return refused(stage, said);
}
