int counter = 0;
int max_val = 100;
void inc(void) {
  counter = counter + 1;
}
int over_max(int x) {
  if (x > max_val) return 1;
  return 0;
}
int main(void) {
  inc();
  inc();
  if (over_max(counter)) return 0;
  return counter;
}
