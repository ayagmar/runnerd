//! A sealed struct written only by its own methods, several of them: the shape the rules ask for.

pub struct Methods {
    o: Option<u64>,
    p: Vec<u64>,
}

impl Methods {
    pub fn start(&mut self) {
        self.o = Some(1);
    }

    pub fn note(&mut self, value: u64) {
        self.p.push(value);
    }

    pub fn end(&mut self) -> Option<u64> {
        self.p.clear();
        self.o.take()
    }
}

#[cfg(test)]
mod tests {
    use super::Methods;

    fn a_test_may_write_it(methods: &mut Methods) {
        methods.o = None;
        methods.p.clear();
    }
}
