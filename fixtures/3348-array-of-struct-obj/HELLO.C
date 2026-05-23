struct Pt { int x; int y; };

struct Pt arr[3] = {{1, 2}, {3, 4}, {5, 6}};

int pick_x(int i) {
  return arr[i].x;
}
