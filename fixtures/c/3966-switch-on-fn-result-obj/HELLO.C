int side_effect_count = 0;
int next(void) {
  side_effect_count = side_effect_count + 1;
  return 2;
}
int main(void) {
  switch (next()) {
    case 1: return 10;
    case 2: return 20;
    case 3: return 30;
  }
  return 0;
}
