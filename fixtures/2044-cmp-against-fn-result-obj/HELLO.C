int get_threshold(void) { return 50; }
int main(void) {
  int x = 60;
  if (x > get_threshold()) return 1;
  return 0;
}
