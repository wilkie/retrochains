static int counter = 0;
static int tick(void) {
  counter = counter + 1;
  return counter;
}
int main(void) {
  tick();
  tick();
  return tick();
}
