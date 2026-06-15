int matrix[2][3] = { { 1, 2, 3 }, { 4, 5, 6 } };
int main(void) {
  int (*row)[3];
  row = &matrix[1];
  return (*row)[2];
}
