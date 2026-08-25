use crate::{Connectivity, StormError};

#[derive(Clone, Debug)]
pub(crate) struct Run {
    pub row: usize,
    pub start: usize,
    pub end: usize,
    parent: usize,
    pub maximum_dbz: f32,
    pub sum_x: f64,
    pub sum_y: f64,
}

impl Run {
    pub fn gate_count(&self) -> usize {
        self.end - self.start + 1
    }
}

#[derive(Debug)]
pub(crate) struct Component {
    pub run_indices: Vec<usize>,
    pub minimum_linear_index: usize,
    pub min_x: usize,
    pub max_x: usize,
    pub min_y: usize,
    pub max_y: usize,
    pub gate_count: usize,
    pub maximum_dbz: f32,
    pub sum_x: f64,
    pub sum_y: f64,
}

impl Component {
    fn from_run(index: usize, run: &Run, nx: usize) -> Self {
        Self {
            run_indices: vec![index],
            minimum_linear_index: run.row * nx + run.start,
            min_x: run.start,
            max_x: run.end,
            min_y: run.row,
            max_y: run.row,
            gate_count: run.gate_count(),
            maximum_dbz: run.maximum_dbz,
            sum_x: run.sum_x,
            sum_y: run.sum_y,
        }
    }

    fn add_run(&mut self, index: usize, run: &Run, nx: usize) -> Result<(), StormError> {
        try_push(&mut self.run_indices, index, "component run indices")?;
        self.minimum_linear_index = self.minimum_linear_index.min(run.row * nx + run.start);
        self.min_x = self.min_x.min(run.start);
        self.max_x = self.max_x.max(run.end);
        self.min_y = self.min_y.min(run.row);
        self.max_y = self.max_y.max(run.row);
        self.gate_count = self
            .gate_count
            .checked_add(run.gate_count())
            .ok_or(StormError::GridSizeOverflow)?;
        self.maximum_dbz = self.maximum_dbz.max(run.maximum_dbz);
        self.sum_x += run.sum_x;
        self.sum_y += run.sum_y;
        Ok(())
    }
}

pub(crate) struct Components {
    pub runs: Vec<Run>,
    pub components: Vec<Component>,
    pub missing_sample_count: usize,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn label_components(
    values: &[f32],
    nx: usize,
    ny: usize,
    x_axis: &[f64],
    y_axis: &[f64],
    threshold: f32,
    minimum_valid_dbz: f32,
    maximum_valid_dbz: f32,
    connectivity: Connectivity,
) -> Result<Components, StormError> {
    let mut runs = Vec::<Run>::new();
    let mut previous_row = 0..0;
    let mut missing_sample_count = 0_usize;

    for (row, &row_y) in y_axis.iter().enumerate().take(ny) {
        let row_begin = runs.len();
        let data_row = row * nx;
        let mut x = 0_usize;
        while x < nx {
            while x < nx {
                let value = values[data_row + x];
                if is_active(value, threshold, minimum_valid_dbz, maximum_valid_dbz) {
                    break;
                }
                missing_sample_count +=
                    usize::from(is_missing(value, minimum_valid_dbz, maximum_valid_dbz));
                x += 1;
            }
            if x == nx {
                break;
            }

            let start = x;
            let mut maximum_dbz = f32::NEG_INFINITY;
            let mut sum_x = 0.0_f64;
            while x < nx {
                let value = values[data_row + x];
                if !is_active(value, threshold, minimum_valid_dbz, maximum_valid_dbz) {
                    break;
                }
                maximum_dbz = maximum_dbz.max(value);
                sum_x += x_axis[x];
                x += 1;
            }
            let end = x - 1;
            let gate_count = end - start + 1;
            let index = runs.len();
            try_push(
                &mut runs,
                Run {
                    row,
                    start,
                    end,
                    parent: index,
                    maximum_dbz,
                    sum_x,
                    sum_y: row_y * gate_count as f64,
                },
                "connected-component runs",
            )?;
        }

        let current_row = row_begin..runs.len();
        union_adjacent_rows(
            &mut runs,
            previous_row.clone(),
            current_row.clone(),
            connectivity,
        );
        previous_row = current_row;
    }

    let mut roots = Vec::new();
    roots
        .try_reserve_exact(runs.len())
        .map_err(|_| StormError::Allocation {
            resource: "component roots",
            requested: runs.len(),
        })?;
    for index in 0..runs.len() {
        roots.push(find_root_compress(&mut runs, index));
    }

    let mut by_root: Vec<Option<Component>> = Vec::new();
    by_root
        .try_reserve_exact(runs.len())
        .map_err(|_| StormError::Allocation {
            resource: "component aggregation slots",
            requested: runs.len(),
        })?;
    by_root.resize_with(runs.len(), || None);

    for (index, &root) in roots.iter().enumerate() {
        let run = &runs[index];
        match &mut by_root[root] {
            Some(component) => component.add_run(index, run, nx)?,
            slot @ None => *slot = Some(Component::from_run(index, run, nx)),
        }
    }

    let mut components = Vec::new();
    components
        .try_reserve_exact(by_root.iter().filter(|slot| slot.is_some()).count())
        .map_err(|_| StormError::Allocation {
            resource: "connected components",
            requested: by_root.len(),
        })?;
    components.extend(by_root.into_iter().flatten());
    components.sort_by_key(|component| component.minimum_linear_index);

    Ok(Components {
        runs,
        components,
        missing_sample_count,
    })
}

fn is_missing(value: f32, minimum_valid_dbz: f32, maximum_valid_dbz: f32) -> bool {
    !value.is_finite() || !(minimum_valid_dbz..=maximum_valid_dbz).contains(&value)
}

fn is_active(value: f32, threshold: f32, minimum_valid_dbz: f32, maximum_valid_dbz: f32) -> bool {
    !is_missing(value, minimum_valid_dbz, maximum_valid_dbz) && value >= threshold
}

fn union_adjacent_rows(
    runs: &mut [Run],
    previous: std::ops::Range<usize>,
    current: std::ops::Range<usize>,
    connectivity: Connectivity,
) {
    let margin = match connectivity {
        Connectivity::Four => 0,
        Connectivity::Eight => 1,
    };
    let mut previous_index = previous.start;

    for current_index in current {
        while previous_index < previous.end
            && runs[previous_index].end.saturating_add(margin) < runs[current_index].start
        {
            previous_index += 1;
        }

        let mut candidate = previous_index;
        while candidate < previous.end
            && runs[candidate].start <= runs[current_index].end.saturating_add(margin)
        {
            union_roots(runs, candidate, current_index);
            candidate += 1;
        }
    }
}

fn find_root(runs: &[Run], mut index: usize) -> usize {
    while runs[index].parent != index {
        index = runs[index].parent;
    }
    index
}

fn find_root_compress(runs: &mut [Run], index: usize) -> usize {
    let root = find_root(runs, index);
    let mut cursor = index;
    while runs[cursor].parent != cursor {
        let next = runs[cursor].parent;
        runs[cursor].parent = root;
        cursor = next;
    }
    root
}

fn union_roots(runs: &mut [Run], a: usize, b: usize) {
    let a = find_root(runs, a);
    let b = find_root(runs, b);
    if a == b {
        return;
    }
    let root = a.min(b);
    let child = a.max(b);
    runs[child].parent = root;
}

pub(crate) fn try_push<T>(
    values: &mut Vec<T>,
    value: T,
    resource: &'static str,
) -> Result<(), StormError> {
    if values.len() == values.capacity() {
        values.try_reserve(1).map_err(|_| StormError::Allocation {
            resource,
            requested: values.len().saturating_add(1),
        })?;
    }
    values.push(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagonal_connectivity_is_explicit() {
        let values = [40.0, 0.0, 0.0, 40.0];
        let axis = [0.0, 1.0];
        let four = label_components(
            &values,
            2,
            2,
            &axis,
            &axis,
            35.0,
            -100.0,
            200.0,
            Connectivity::Four,
        )
        .unwrap();
        let eight = label_components(
            &values,
            2,
            2,
            &axis,
            &axis,
            35.0,
            -100.0,
            200.0,
            Connectivity::Eight,
        )
        .unwrap();
        assert_eq!(four.components.len(), 2);
        assert_eq!(eight.components.len(), 1);
    }
}
