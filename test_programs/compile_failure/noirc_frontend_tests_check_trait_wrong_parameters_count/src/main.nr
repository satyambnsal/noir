
    trait Default2 {
        fn default(x: Field, y: Field) -> Self;
    }

    struct Foo {
        bar: Field,
        array: [Field; 2],
    }

    impl Default2 for Foo {
        fn default(x: Field) -> Self {
            Self { bar: x, array: [x, x] }
        }
    }

    fn main() {
    }
    