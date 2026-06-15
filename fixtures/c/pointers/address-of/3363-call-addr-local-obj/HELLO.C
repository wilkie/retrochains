void use(int *p);

void driver(void) {
  int local = 7;
  use(&local);
}
