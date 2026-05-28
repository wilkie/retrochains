int main(void) {
  int i;
  int total = 0;
  for (i = 0; i < 10; i = i + 1) {
    if (i == 5) continue;
    total = total + i;
  }
  return total;
}
