from pathlib import Path

from setuptools import Distribution, setup


class BinaryDistribution(Distribution):
    def has_ext_modules(self) -> bool:
        return (Path(__file__).parent / "src" / "executioner_sdk" / "bin").exists()


setup(distclass=BinaryDistribution)
