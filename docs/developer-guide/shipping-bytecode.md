# eBPF Bytecode Image Specifications

## Introduction

The eBPF Bytecode Image specification defines how to package eBPF bytecode
as container images. The initial primary use case focuses on the containerization
and deployment of eBPF programs within container orchestration systems such as
Kubernetes, where it is necessary to provide a portable way to distribute
bytecode to all nodes which need it.

## Specifications

We provide two distinct spec variants here to ensure interoperability with existing registries
and packages which do no support the new custom media types defined here.

- [custom-data-type-spec](#custom-oci-compatible-spec)
- [backwards-compatable-spec](#backwards-compatible-oci-compliant-spec)

## Backwards compatible OCI compliant spec

This variant makes use of existing OCI conventions to represent eBPF Bytecode
as container images.

### Image Layers

The container images following this variant must contain exactly one layer who's
media type is one of the following:

- `application/vnd.oci.image.layer.v1.tar+gzip` or the [compliant](https://github.com/opencontainers/image-spec/tree/main/media-types.md#applicationvndociimagelayerv1targzip) `application/vnd.docker.image.rootfs.diff.tar.gzip`

Additionally the image layer must contain a valid eBPF object file (generally containing
a `.o` extension) placed at the root of the layer `./`.

### Image Labels

To provide relevant metadata regarding the bytecode to any consumers, some relevant labels
**MUST** be defined on the image.

These labels are dynamic and defined as follows:

- `io.ebpf.programs`: A label which defines The eBPF programs stored in the bytecode image.
   the value of the label is a list which must contain a valid JSON object with
   Key's specifying the program name, and values specifying the program type i.e:
   "{ "pass" : "xdp" , "counter" : "tc", ...}".

- `io.ebpf.maps`: A label which defines The eBPF maps stored in the bytecode image.
   the value of the label is a list which must contain a valid JSON object with
   Key's specifying the map name, and values specifying the map type i.e:
   "{ "xdp_stats_map" : "per_cpu_array", ...}".

### Building a Backwards compatible OCI compliant image

Bpfman does not provide wrappers around compilers like clang since many eBPF
libraries (i.e aya, libbpf, cilium-ebpf) already do so, meaning users are expected
to pass in the correct ebpf program bytecode for the appropriate platform. However,
bpfman does provide a few image builder commands to make this whole process easier.

An Example Containerfile can be found at `/packaging/container/deployment/Containerfile.bytecode`

#### Host Platform Image Build

```console
bpfman image build -b ./examples/go-xdp-counter/bpf_bpfel.o -f Containerfile.bytecode --tag quay.io/<USER>/go-xdp-counter
```

Where `./examples/go-xdp-counter/bpf_bpfel.o` is the directory the bytecode object file is located.

Users can also use `skopeo` to ensure the image follows the
backwards compatible version of the spec:

- `skopeo inspect` will show the correctly configured labels stored in the
  configuration layer (`application/vnd.oci.image.config.v1+json`) of the image.

```bash
skopeo inspect docker://quay.io/bpfman-bytecode/go-xdp-counter
{
    "Name": "quay.io/bpfman-bytecode/go-xdp-counter",
    "Digest": "sha256:e8377e94c56272937689af88a1a6231d4d594f83218b5cda839eaeeea70a30d3",
    "RepoTags": [
        "latest"
    ],
    "Created": "2024-05-30T09:17:15.327378016-04:00",
    "DockerVersion": "",
    "Labels": {
        "io.ebpf.maps": "{\"xdp_stats_map\":\"per_cpu_array\"}",
        "io.ebpf.programs": "{\"xdp_stats\":\"xdp\"}"
    },
    "Architecture": "amd64",
    "Os": "linux",
    "Layers": [
        "sha256:c0d921d3f0d077da7cdfba8c0240fb513789e7698cdf326f80f30f388c084cff"
    ],
    "LayersData": [
        {
            "MIMEType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
            "Digest": "sha256:c0d921d3f0d077da7cdfba8c0240fb513789e7698cdf326f80f30f388c084cff",
            "Size": 2656,
            "Annotations": null
        }
    ],
    "Env": [
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    ]
}
```

## Custom OCI compatible spec

This variant of the eBPF bytecode image spec uses custom OCI medium types
to represent eBPF bytecode as container images. Many toolchains and registries
may not support this yet.

TODO
